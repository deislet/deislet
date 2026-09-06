//! Opaque, query-bound cursors shared by every Control list API.
//!
//! The client token is 256 random bits. Only its SHA-256 digest is stored;
//! sort positions and query bindings stay in the Control database. A token
//! therefore cannot become an accidental public encoding of SQLite columns,
//! and possession never grants access: the normal route authorization and
//! the query binding are checked again on every request.
//!
//! Cursor persistence is deliberately independent of any one resource
//! store: catalog, identity and audit lists all page through the same
//! `api_page_cursors` table, so each of those stores hands out a
//! [`PageCursorStore`] built on the database it already owns.

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use super::{CatalogError, CatalogResult, now_secs};
use crate::database::Database;

const TOKEN_BYTES: usize = 32;
const TOKEN_TEXT_BYTES: usize = 43;
/// How long an issued cursor stays resolvable. Public contract
/// (docs/adr/016, list and query section): three days.
pub const CURSOR_TTL_SECONDS: i64 = 3 * 24 * 60 * 60;

/// What every refusal of a cursor says, whichever way it failed: a token
/// that is not one of ours, one that expired, and one issued for another
/// collection or another set of filters are one answer on purpose, because
/// telling them apart would say whether a guessed token exists.
///
/// One sentence in one place: it is the legacy body of every list route
/// that refuses a cursor, and `crate::api::error` reads it to answer
/// `cursor-invalid` on the structured surfaces without the message moving.
pub const INVALID_CURSOR_MESSAGE: &str = "cursor is invalid, expired, or belongs to another query";
const MAX_COLLECTION_BYTES: usize = 128;
/// Upper bound for each textual keyset component. Identities are canonical
/// ids today, but a secondary sort column may carry a display name or a
/// hostname, so the bound leaves room without becoming a storage sink.
const MAX_POSITION_IDENTITY_BYTES: usize = 1024;

/// The keyset position a cursor resumes from.
///
/// `sort_value` and `identity` form the versioned composite sort every
/// migrated list uses today. `secondary` is reserved for lists whose sort
/// needs a second textual column between them (for example a name sorted
/// before the id tie-breaker); it is omitted from stored JSON when unused so
/// rows written before it existed keep decoding unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PagePosition {
    pub sort_value: i64,
    pub identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPage<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub has_more: bool,
}

/// Capacity view of the cursor table at one instant.
///
/// Every list request that has a further page writes one row, and a row
/// lives for [`CURSOR_TTL_SECONDS`], so `live` is a direct reading of how
/// many partial traversals clients are holding open. `expired_pending_removal`
/// stays near zero while the liveness sweep runs; a growing value means the
/// sweep is not reaching this table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PageCursorStatistics {
    /// Cursors a client can still resume from.
    pub live: usize,
    /// Cursors past their expiry that no sweep has removed yet.
    pub expired_pending_removal: usize,
    /// Seconds since the oldest still-resolvable cursor was issued; `None`
    /// when there is no live cursor.
    pub oldest_live_age_seconds: Option<i64>,
}

pub(crate) fn sqlite_fetch_limit(limit: usize, resource: &str) -> CatalogResult<i64> {
    if !(1..=1_000).contains(&limit) {
        return Err(CatalogError::invalid(format!(
            "{resource} limit must be between 1 and 1000"
        )));
    }
    let fetch = limit
        .checked_add(1)
        .ok_or_else(|| CatalogError::invalid(format!("{resource} limit is too large")))?;
    i64::try_from(fetch)
        .map_err(|_| CatalogError::invalid(format!("{resource} limit exceeds SQLite range")))
}

pub(crate) fn finish_page<T>(
    mut items: Vec<T>,
    limit: usize,
    total: i64,
    resource: &str,
) -> CatalogResult<CatalogPage<T>> {
    let has_more = items.len() > limit;
    items.truncate(limit);
    let total = usize::try_from(total)
        .map_err(|_| CatalogError::Database(rusqlite::Error::IntegralValueOutOfRange(0, total)))?;
    if items.len() > total {
        return Err(CatalogError::invalid(format!(
            "{resource} page contains more rows than its total"
        )));
    }
    Ok(CatalogPage {
        items,
        total,
        has_more,
    })
}

/// Persistence for opaque list cursors, shared by every resource store that
/// lives in the Control database. Cheap to construct: it holds only the
/// database handle, so stores hand one out per request.
#[derive(Clone)]
pub struct PageCursorStore {
    db: Arc<Database>,
}

impl PageCursorStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Resolve a client token only for the exact collection, scope, filter,
    /// and sort definition that issued it. Page size is intentionally absent
    /// from `query_binding`, so callers may change `limit` between pages.
    pub async fn resolve(
        &self,
        token: &str,
        collection: &str,
        query_binding: &str,
    ) -> CatalogResult<PagePosition> {
        validate_collection(collection)?;
        let raw_token = decode_token(token)?;
        let token_sha256 = sha256_hex(&raw_token);
        let binding_sha256 = binding_sha256(collection, query_binding);
        let collection = collection.to_string();
        let now = now_secs();
        self.db
            .with_connection(move |connection| {
                let position_json = connection
                    .query_row(
                        "SELECT position_json FROM api_page_cursors
                          WHERE token_sha256 = ?1 AND collection = ?2
                            AND query_sha256 = ?3 AND expires_at > ?4",
                        params![token_sha256, collection, binding_sha256, now],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                let Some(position_json) = position_json else {
                    debug!(
                        collection,
                        "list cursor rejected: unknown, expired, or bound to another query"
                    );
                    return Err(invalid_cursor());
                };
                let position: PagePosition =
                    serde_json::from_str(&position_json).map_err(|_| invalid_cursor())?;
                validate_position(&position)?;
                Ok(position)
            })
            .await
    }

    /// Persist the next keyset position and return a random URL-safe token.
    /// Expired cursors are opportunistically removed in the same transaction.
    pub async fn issue(
        &self,
        collection: &str,
        query_binding: &str,
        position: PagePosition,
    ) -> CatalogResult<String> {
        validate_collection(collection)?;
        validate_position(&position)?;
        let mut raw_token = [0_u8; TOKEN_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut raw_token);
        let token = URL_SAFE_NO_PAD.encode(raw_token);
        debug_assert_eq!(token.len(), TOKEN_TEXT_BYTES);
        let token_sha256 = sha256_hex(&raw_token);
        let binding_sha256 = binding_sha256(collection, query_binding);
        let position_json = serde_json::to_string(&position).map_err(|error| {
            CatalogError::invalid(format!("cannot encode pagination position: {error}"))
        })?;
        let now = now_secs();
        let expires_at = now.saturating_add(CURSOR_TTL_SECONDS);
        let collection = collection.to_string();
        self.db
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                transaction
                    .execute("DELETE FROM api_page_cursors WHERE expires_at <= ?1", [now])?;
                transaction.execute(
                    "INSERT INTO api_page_cursors
                       (token_sha256, collection, query_sha256, position_json,
                        created_at, expires_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        token_sha256,
                        collection,
                        binding_sha256,
                        position_json,
                        now,
                        expires_at,
                    ],
                )?;
                transaction.commit()?;
                Ok(token)
            })
            .await
    }

    /// Remove every cursor whose expiry is at or before `now`; returns how
    /// many rows went.
    ///
    /// The same statement [`PageCursorStore::issue`] runs opportunistically,
    /// exposed so a background sweep can keep the table bounded during a
    /// quiet period when no list request is issuing anything. Idempotent
    /// and safe to run on every tick: it walks the expiry index only as far
    /// as the first live row.
    pub async fn expire(&self, now: i64) -> CatalogResult<usize> {
        let removed = self
            .db
            .with_connection(move |connection| {
                let removed = connection
                    .execute("DELETE FROM api_page_cursors WHERE expires_at <= ?1", [now])?;
                Ok::<_, CatalogError>(removed)
            })
            .await?;
        if removed == 0 {
            debug!("no expired list cursors to remove");
        } else {
            info!(
                removed,
                "removed {removed} expired list cursor(s); a client still holding one of them \
                 receives a bad-request answer and must restart its traversal"
            );
        }
        Ok(removed)
    }

    /// One reading of the cursor table for the metrics endpoints.
    ///
    /// Three probes of `idx_api_page_cursors_expiry_v1`, each bounded by the
    /// expiry value: two range counts and one seek to the oldest live row.
    /// Nothing here reads a token digest or a position.
    pub async fn statistics(&self, now: i64) -> CatalogResult<PageCursorStatistics> {
        self.db
            .with_connection(move |connection| {
                let (live, expired, oldest_created_at) = connection.query_row(
                    "SELECT
                        (SELECT COUNT(*) FROM api_page_cursors WHERE expires_at > ?1),
                        (SELECT COUNT(*) FROM api_page_cursors WHERE expires_at <= ?1),
                        (SELECT created_at FROM api_page_cursors WHERE expires_at > ?1
                          ORDER BY expires_at ASC, token_sha256 ASC LIMIT 1)",
                    [now],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                        ))
                    },
                )?;
                let count = |value: i64| {
                    usize::try_from(value).map_err(|_| {
                        CatalogError::Database(rusqlite::Error::IntegralValueOutOfRange(0, value))
                    })
                };
                Ok(PageCursorStatistics {
                    live: count(live)?,
                    expired_pending_removal: count(expired)?,
                    oldest_live_age_seconds: oldest_created_at
                        .map(|created_at| now.saturating_sub(created_at).max(0)),
                })
            })
            .await
    }
}

fn decode_token(token: &str) -> CatalogResult<[u8; TOKEN_BYTES]> {
    if token.len() != TOKEN_TEXT_BYTES
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(invalid_cursor());
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| invalid_cursor())?;
    decoded.try_into().map_err(|_| invalid_cursor())
}

fn validate_collection(collection: &str) -> CatalogResult<()> {
    if collection.is_empty()
        || collection.len() > MAX_COLLECTION_BYTES
        || !collection.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
    {
        return Err(CatalogError::invalid(
            "pagination collection identifier is invalid",
        ));
    }
    Ok(())
}

fn validate_position(position: &PagePosition) -> CatalogResult<()> {
    if position.identity.is_empty() || !position_text_is_valid(&position.identity) {
        return Err(invalid_cursor());
    }
    // A secondary sort column may legitimately be empty text (an unnamed
    // row still sorts), so only the size and control-character bounds apply.
    if position
        .secondary
        .as_deref()
        .is_some_and(|secondary| !position_text_is_valid(secondary))
    {
        return Err(invalid_cursor());
    }
    Ok(())
}

fn position_text_is_valid(text: &str) -> bool {
    text.len() <= MAX_POSITION_IDENTITY_BYTES && !text.contains(['\r', '\n', '\0'])
}

fn binding_sha256(collection: &str, query_binding: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"deislet-control-page-cursor-v1\0");
    hasher.update(collection.as_bytes());
    hasher.update(b"\0");
    hasher.update(query_binding.as_bytes());
    hex::encode(hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn invalid_cursor() -> CatalogError {
    CatalogError::invalid(INVALID_CURSOR_MESSAGE)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::database::Database;

    #[tokio::test]
    async fn cursor_is_opaque_reusable_query_bound_and_expirable() {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let store = PageCursorStore::new(database.clone());
        let position = PagePosition {
            sort_value: 17,
            identity: "0199b603-20bd-7342-9be3-cef10620d8bb".to_string(),
            secondary: None,
        };
        let token = store
            .issue("provider-targets", "installation=one", position.clone())
            .await
            .unwrap();
        assert_eq!(token.len(), TOKEN_TEXT_BYTES);
        assert!(!token.contains("0199b603"));
        assert_eq!(
            store
                .resolve(&token, "provider-targets", "installation=one")
                .await
                .unwrap(),
            position
        );
        assert_eq!(
            store
                .resolve(&token, "provider-targets", "installation=one")
                .await
                .unwrap(),
            position
        );
        assert!(
            store
                .resolve(&token, "provider-targets", "installation=two")
                .await
                .is_err()
        );
        assert!(
            store
                .resolve(&token, "provider-installations", "installation=one")
                .await
                .is_err()
        );
        let token_sha256 = sha256_hex(&decode_token(&token).unwrap());
        let binding_sha256 = binding_sha256("provider-targets", "installation=one");
        let position_json = serde_json::to_string(&position).unwrap();
        database
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                transaction.execute(
                    "DELETE FROM api_page_cursors WHERE token_sha256 = ?1",
                    [&token_sha256],
                )?;
                transaction.execute(
                    "INSERT INTO api_page_cursors
                       (token_sha256, collection, query_sha256, position_json,
                        created_at, expires_at)
                     VALUES (?1, 'provider-targets', ?2, ?3, 0, 1)",
                    params![token_sha256, binding_sha256, position_json],
                )?;
                transaction.commit()?;
                Ok::<_, crate::database::DatabaseError>(())
            })
            .await
            .unwrap();
        assert!(
            store
                .resolve(&token, "provider-targets", "installation=one")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn every_store_shares_one_cursor_table() {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let catalog = crate::catalog::CatalogStore::new(database.clone());
        let identity = crate::identity::IdentityStore::new(database.clone());
        let audit = crate::audit::AuditStore::new(database);
        let position = PagePosition {
            sort_value: 5,
            identity: "0199b603-20bd-7342-9be3-cef10620d8bb".to_string(),
            secondary: None,
        };
        let token = catalog
            .page_cursors()
            .issue("accounts", "scope=installation;sort=v1", position.clone())
            .await
            .unwrap();
        assert_eq!(
            identity
                .page_cursors()
                .resolve(&token, "accounts", "scope=installation;sort=v1")
                .await
                .unwrap(),
            position
        );
        assert_eq!(
            audit
                .page_cursors()
                .resolve(&token, "accounts", "scope=installation;sort=v1")
                .await
                .unwrap(),
            position
        );
    }

    #[test]
    fn stored_positions_without_secondary_still_decode() {
        let position: PagePosition = serde_json::from_str(
            r#"{"sortValue":42,"identity":"0199b603-20bd-7342-9be3-cef10620d8bb"}"#,
        )
        .unwrap();
        assert_eq!(
            position,
            PagePosition {
                sort_value: 42,
                identity: "0199b603-20bd-7342-9be3-cef10620d8bb".to_string(),
                secondary: None,
            }
        );
        // Positions without a secondary keep the exact JSON shape they had
        // before the field existed, so nothing rewrites old rows.
        assert_eq!(
            serde_json::to_string(&position).unwrap(),
            r#"{"sortValue":42,"identity":"0199b603-20bd-7342-9be3-cef10620d8bb"}"#
        );
        assert!(
            serde_json::from_str::<PagePosition>(
                r#"{"sortValue":42,"identity":"x","unexpected":true}"#
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn secondary_sort_column_round_trips_through_a_cursor() {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let store = PageCursorStore::new(database);
        let position = PagePosition {
            sort_value: 0,
            identity: "0199b603-20bd-7342-9be3-cef10620d8bb".to_string(),
            secondary: Some("release-manager".to_string()),
        };
        let json = serde_json::to_string(&position).unwrap();
        assert!(json.contains(r#""secondary":"release-manager""#));
        assert_eq!(
            serde_json::from_str::<PagePosition>(&json).unwrap(),
            position
        );
        let token = store
            .issue(
                "accounts",
                "scope=installation;sort=name-asc,id-asc-v1",
                position.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .resolve(
                    &token,
                    "accounts",
                    "scope=installation;sort=name-asc,id-asc-v1"
                )
                .await
                .unwrap(),
            position
        );
    }

    #[test]
    fn position_text_bounds_apply_to_both_columns() {
        let base = PagePosition {
            sort_value: 1,
            identity: "id".to_string(),
            secondary: None,
        };
        assert!(validate_position(&base).is_ok());
        let long = "a".repeat(MAX_POSITION_IDENTITY_BYTES);
        assert!(
            validate_position(&PagePosition {
                identity: long.clone(),
                ..base.clone()
            })
            .is_ok()
        );
        assert!(
            validate_position(&PagePosition {
                identity: format!("{long}a"),
                ..base.clone()
            })
            .is_err()
        );
        assert!(
            validate_position(&PagePosition {
                secondary: Some(long.clone()),
                ..base.clone()
            })
            .is_ok()
        );
        assert!(
            validate_position(&PagePosition {
                secondary: Some(format!("{long}a")),
                ..base.clone()
            })
            .is_err()
        );
        assert!(
            validate_position(&PagePosition {
                secondary: Some("line\nbreak".to_string()),
                ..base.clone()
            })
            .is_err()
        );
        assert!(
            validate_position(&PagePosition {
                secondary: Some(String::new()),
                ..base
            })
            .is_ok()
        );
    }

    #[test]
    fn malformed_cursor_never_reaches_storage() {
        for token in ["", "abc", "a/b", &"a".repeat(TOKEN_TEXT_BYTES + 1)] {
            assert!(decode_token(token).is_err());
        }
    }

    /// Write a cursor row directly with chosen timestamps, bypassing the
    /// issue path so a test can place rows on either side of `now`.
    async fn insert_cursor_row(
        database: &Database,
        digest: &str,
        created_at: i64,
        expires_at: i64,
    ) {
        let digest = digest.to_string();
        database
            .with_connection(move |connection| {
                connection.execute(
                    "INSERT INTO api_page_cursors
                       (token_sha256, collection, query_sha256, position_json,
                        created_at, expires_at)
                     VALUES (?1, 'accounts', ?2, '{\"sortValue\":1,\"identity\":\"one\"}',
                             ?3, ?4)",
                    params![digest, "b".repeat(64), created_at, expires_at],
                )?;
                Ok::<_, crate::database::DatabaseError>(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn expire_removes_only_rows_past_their_deadline() {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let store = PageCursorStore::new(database.clone());
        let now = 1_000_000;
        insert_cursor_row(&database, &"1".repeat(64), now - 400_000, now - 140_800).await;
        insert_cursor_row(&database, &"2".repeat(64), now - 259_200, now).await;
        insert_cursor_row(&database, &"3".repeat(64), now - 10, now + 259_190).await;

        assert_eq!(store.expire(now).await.unwrap(), 2);
        assert_eq!(store.expire(now).await.unwrap(), 0);
        let statistics = store.statistics(now).await.unwrap();
        assert_eq!(
            statistics,
            PageCursorStatistics {
                live: 1,
                expired_pending_removal: 0,
                oldest_live_age_seconds: Some(10),
            }
        );
    }

    #[tokio::test]
    async fn statistics_separate_live_from_expired_and_age_the_oldest_live_row() {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let store = PageCursorStore::new(database.clone());
        let now = 2_000_000;
        assert_eq!(
            store.statistics(now).await.unwrap(),
            PageCursorStatistics::default()
        );
        // Two expired rows, three live ones; the oldest live row was issued
        // 5000 seconds ago and is the one the age has to report even though
        // a newer row sorts first by digest.
        insert_cursor_row(&database, &"a".repeat(64), now - 300_000, now - 40_800).await;
        insert_cursor_row(&database, &"b".repeat(64), now - 259_201, now - 1).await;
        insert_cursor_row(&database, &"0".repeat(64), now - 100, now + 259_100).await;
        insert_cursor_row(&database, &"c".repeat(64), now - 5_000, now + 254_200).await;
        insert_cursor_row(&database, &"d".repeat(64), now - 2_500, now + 256_700).await;
        assert_eq!(
            store.statistics(now).await.unwrap(),
            PageCursorStatistics {
                live: 3,
                expired_pending_removal: 2,
                oldest_live_age_seconds: Some(5_000),
            }
        );
        // A row issued by a clock that ran ahead of this one never reports
        // a negative age.
        insert_cursor_row(&database, &"e".repeat(64), now + 60, now + 259_260).await;
        insert_cursor_row(&database, &"f".repeat(64), now + 90, now + 100).await;
        assert_eq!(
            store.statistics(now + 200).await.unwrap(),
            PageCursorStatistics {
                live: 4,
                expired_pending_removal: 3,
                oldest_live_age_seconds: Some(5_200),
            }
        );
        let only_future = PageCursorStore::new(Arc::new(Database::init(":memory:").await.unwrap()));
        insert_cursor_row(&only_future.db, &"9".repeat(64), now + 60, now + 259_260).await;
        assert_eq!(
            only_future
                .statistics(now)
                .await
                .unwrap()
                .oldest_live_age_seconds,
            Some(0)
        );
    }

    #[tokio::test]
    async fn issued_cursors_appear_live_and_the_sweep_query_uses_the_expiry_index() {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let store = PageCursorStore::new(database.clone());
        let position = PagePosition {
            sort_value: 1,
            identity: "0199b603-20bd-7342-9be3-cef10620d8bb".to_string(),
            secondary: None,
        };
        store
            .issue("accounts", "scope=installation", position)
            .await
            .unwrap();
        let statistics = store.statistics(now_secs()).await.unwrap();
        assert_eq!(statistics.live, 1);
        assert_eq!(statistics.expired_pending_removal, 0);
        assert!(statistics.oldest_live_age_seconds.is_some());

        let plans = database
            .with_connection(|connection| {
                let mut plans = Vec::new();
                for sql in [
                    "DELETE FROM api_page_cursors WHERE expires_at <= ?1",
                    "SELECT COUNT(*) FROM api_page_cursors WHERE expires_at > ?1",
                    "SELECT created_at FROM api_page_cursors WHERE expires_at > ?1
                      ORDER BY expires_at ASC, token_sha256 ASC LIMIT 1",
                ] {
                    let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
                    let lines = statement
                        .query_map(rusqlite::params_from_iter(nulls), |row| {
                            row.get::<_, String>(3)
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    plans.push(lines.join("\n"));
                }
                Ok::<_, crate::database::DatabaseError>(plans)
            })
            .await
            .unwrap();
        for plan in plans {
            assert!(
                plan.contains("idx_api_page_cursors_expiry_v1"),
                "cursor maintenance query does not use the expiry index: {plan}"
            );
            assert!(!plan.contains("TEMP B-TREE"), "{plan}");
        }
    }
}
