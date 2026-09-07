//! Opaque, query-bound cursors shared by every Control list API.
//!
//! The client token is 256 bits derived under a key this process draws at
//! startup and never hands out ([`CURSOR_TOKEN_KEY`]), so it is as
//! unguessable as a random one and, unlike a random one, the same page of
//! the same query always yields the same token - which is what keeps a
//! client polling one page from writing a row per poll. Only the token's
//! SHA-256 digest is stored; sort positions and query bindings stay in the
//! Control database. A token therefore cannot become an accidental public
//! encoding of SQLite columns, and possession never grants access: the
//! normal route authorization and the query binding are checked again on
//! every request.
//!
//! Cursor persistence is deliberately independent of any one resource
//! store: catalog, identity and audit lists all page through the same
//! `api_page_cursors` table, so each of those stores hands out a
//! [`PageCursorStore`] built on the database it already owns.

use std::sync::{Arc, LazyLock};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rand::RngCore;
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use super::{CatalogError, CatalogResult, now_secs};
use crate::database::Database;

const TOKEN_BYTES: usize = 32;
/// Domain separator for the token derivation, so a digest computed here can
/// never coincide with one computed for anything else under the same key.
const TOKEN_DOMAIN: &str = "deislet-control-list-cursor-v1";
const TOKEN_TEXT_BYTES: usize = 43;
/// How long an issued cursor stays resolvable. Public contract
/// (docs/adr/016, list and query section): three days.
pub const CURSOR_TTL_SECONDS: i64 = 3 * 24 * 60 * 60;

/// How many expired cursors one expiry statement removes.
///
/// Expiry runs on the process's single connection, the same one every list
/// request and every heartbeat write needs, so the question is not how many
/// rows a pass removes but how long the longest single statement holds that
/// connection. An unbounded `DELETE` is free in the ordinary case, where it
/// finds nothing - and unbounded in exactly the case the sweep exists for:
/// a table that filled up and then went quiet for longer than a cursor
/// lives. Its whole backlog would come out in one statement, with every
/// request queued behind it, and the same statement runs inside a list
/// request when that request is the first thing to touch the table. A fixed
/// slice at a time costs a few more statements and gives the connection
/// back between them, so the longest pause is the slice and not the
/// backlog.
const EXPIRY_BATCH: usize = 512;

/// The expiry statement, in the one place both callers take it from: the
/// oldest expired cursors first, at most [`EXPIRY_BATCH`] of them. The
/// subquery is the seek - `idx_api_page_cursors_expiry_v1` is ordered by
/// expiry, so it stops at the first live row - and the delete then removes
/// those rows by their primary key.
const EXPIRY_BATCH_SQL: &str = "DELETE FROM api_page_cursors
                                 WHERE token_sha256 IN (
                                       SELECT token_sha256 FROM api_page_cursors
                                        WHERE expires_at <= ?1
                                        ORDER BY expires_at ASC
                                        LIMIT ?2)";

/// Remove at most one slice of expired cursors; returns how many went, so a
/// caller can tell a short slice (nothing left) from a full one.
fn delete_expired_cursor_batch(
    connection: &rusqlite::Connection,
    now: i64,
) -> rusqlite::Result<usize> {
    connection.execute(EXPIRY_BATCH_SQL, params![now, EXPIRY_BATCH as i64])
}

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
/// One row per *page position* that has been handed out and not yet expired:
/// [`PageCursorStore::issue`] derives a token from the page it resumes, so
/// repeated answers about the same page share a row rather than each adding
/// one. `live` is therefore how many distinct places in how many collections
/// clients can still resume from - the number that grows with real
/// traversals and not with polling. `expired_pending_removal` stays near
/// zero while the liveness sweep runs; a growing value means the sweep is
/// not reaching this table.
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

/// Which way a page walks one column of its sort.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeysetDirection {
    Ascending,
    Descending,
}

impl KeysetDirection {
    /// The comparison that means "strictly after the cursor" for this
    /// direction.
    const fn comparison(self) -> &'static str {
        match self {
            Self::Ascending => ">",
            Self::Descending => "<",
        }
    }
}

/// One column of a keyset sort and the position value the page resumes
/// after.
pub(crate) struct KeysetBound {
    column: String,
    direction: KeysetDirection,
    value: Value,
}

impl KeysetBound {
    pub(crate) fn new(column: &str, direction: KeysetDirection, value: Value) -> Self {
        Self {
            column: column.to_owned(),
            direction,
            value,
        }
    }

    pub(crate) fn ascending(column: &str, value: Value) -> Self {
        Self::new(column, KeysetDirection::Ascending, value)
    }

    pub(crate) fn descending(column: &str, value: Value) -> Self {
        Self::new(column, KeysetDirection::Descending, value)
    }
}

/// The cursor half of a page statement: the predicate that resumes after a
/// position, and the parameter index the statement's `LIMIT` takes once the
/// cursor's own parameters are accounted for.
///
/// The predicate has to be *absent* from a first page's statement rather
/// than switched off by a bound `NULL`. SQLite compiles one plan per
/// statement text, and it cannot turn `(?n IS NULL OR sort < ?n OR (sort =
/// ?n AND tie < ?m))` into an index range constraint: the whole disjunction
/// becomes a row filter, the index supplies only the sort order, and
/// resuming a page deep in a large collection walks every row of the scope
/// that precedes the cursor - the ten-thousandth page reads the first
/// million rows again. Written only when the request carries a cursor, the
/// same predicate is what the index seeks to, and every page of a
/// collection costs the same. `database::read_keyset_page`, which serves
/// the diagnostic histories, builds its statements the same way, and
/// `catalog::pagination_plans` explains both forms of every statement
/// against the real schema.
pub(crate) struct KeysetResume {
    predicate: Option<String>,
    values: Vec<Value>,
    limit_index: usize,
}

impl KeysetResume {
    /// A first page: no predicate and no cursor bindings, so `LIMIT` takes
    /// the first free parameter itself.
    pub(crate) fn first_page(first_param: usize) -> Self {
        Self {
            predicate: None,
            values: Vec::new(),
            limit_index: first_param,
        }
    }

    /// Resume strictly after a position, comparing the sort columns in
    /// order: the first column decides, and each further column only
    /// breaks a tie among all the columns before it.
    pub(crate) fn after(first_param: usize, bounds: Vec<KeysetBound>) -> Self {
        debug_assert!(!bounds.is_empty(), "a keyset resume needs a sort column");
        let mut alternatives: Vec<String> = Vec::with_capacity(bounds.len());
        for (index, bound) in bounds.iter().enumerate() {
            let mut terms: Vec<String> = bounds[..index]
                .iter()
                .enumerate()
                .map(|(tie, earlier)| format!("{} = ?{}", earlier.column, first_param + tie))
                .collect();
            terms.push(format!(
                "{} {} ?{}",
                bound.column,
                bound.direction.comparison(),
                first_param + index
            ));
            alternatives.push(if terms.len() == 1 {
                terms.remove(0)
            } else {
                format!("({})", terms.join(" AND "))
            });
        }
        Self::from_parts(first_param, alternatives.join(" OR "), bounds)
    }

    /// Resume strictly after a position with one row-value comparison,
    /// which SQLite seeks through a compound index as a single constraint.
    /// Only usable when every column of the sort runs the same way.
    pub(crate) fn after_row_value(first_param: usize, bounds: Vec<KeysetBound>) -> Self {
        debug_assert!(!bounds.is_empty(), "a keyset resume needs a sort column");
        let direction = bounds[0].direction;
        debug_assert!(
            bounds.iter().all(|bound| bound.direction == direction),
            "a row-value comparison cannot mix sort directions"
        );
        let columns = bounds
            .iter()
            .map(|bound| bound.column.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let placeholders = (0..bounds.len())
            .map(|index| format!("?{}", first_param + index))
            .collect::<Vec<_>>()
            .join(", ");
        let predicate = format!("({columns}) {} ({placeholders})", direction.comparison());
        Self::from_parts(first_param, predicate, bounds)
    }

    fn from_parts(first_param: usize, predicate: String, bounds: Vec<KeysetBound>) -> Self {
        let limit_index = first_param + bounds.len();
        Self {
            predicate: Some(predicate),
            values: bounds.into_iter().map(|bound| bound.value).collect(),
            limit_index,
        }
    }

    /// The form every timestamped list uses: one value column with an
    /// identity tie-breaker, both walked the same way, resumed from
    /// `cursor` (or a first page when there is none).
    pub(crate) fn value_and_identity(
        first_param: usize,
        direction: KeysetDirection,
        value_column: &str,
        identity_column: &str,
        cursor: Option<PagePosition>,
    ) -> Self {
        match cursor {
            Some(position) => Self::after(
                first_param,
                vec![
                    KeysetBound::new(value_column, direction, Value::from(position.sort_value)),
                    KeysetBound::new(identity_column, direction, Value::from(position.identity)),
                ],
            ),
            None => Self::first_page(first_param),
        }
    }

    /// A list whose sort is one column that is already unique, so the
    /// cursor carries nothing but that position.
    pub(crate) fn identity_only(
        first_param: usize,
        direction: KeysetDirection,
        column: &str,
        after: Option<String>,
    ) -> Self {
        match after {
            Some(identity) => Self::after(
                first_param,
                vec![KeysetBound::new(column, direction, Value::from(identity))],
            ),
            None => Self::first_page(first_param),
        }
    }

    /// The predicate as a further condition of a `WHERE` that already has
    /// one, or nothing at all on a first page.
    pub(crate) fn and_clause(&self) -> String {
        match &self.predicate {
            Some(predicate) => format!(" AND ({predicate})"),
            None => String::new(),
        }
    }

    /// The predicate as the whole `WHERE` of a statement that filters on
    /// nothing else, or nothing at all on a first page.
    pub(crate) fn where_clause(&self) -> String {
        match &self.predicate {
            Some(predicate) => format!(" WHERE {predicate}"),
            None => String::new(),
        }
    }

    /// The cursor's own bindings, for a statement that assembles its
    /// parameter list itself.
    pub(crate) fn values(&self) -> &[Value] {
        &self.values
    }

    /// The parameter index the statement's `LIMIT` takes.
    pub(crate) fn limit_index(&self) -> usize {
        self.limit_index
    }

    /// The statement's bindings: the filters the caller names first, then
    /// the cursor's own values, then the fetch limit - the order every page
    /// statement numbers its parameters in.
    pub(crate) fn bindings(
        &self,
        filters: impl IntoIterator<Item = Value>,
        fetch_limit: i64,
    ) -> Vec<Value> {
        let mut bindings: Vec<Value> = filters.into_iter().collect();
        bindings.extend(self.values.iter().cloned());
        bindings.push(Value::from(fetch_limit));
        bindings
    }
}

/// The key every cursor token is derived under.
///
/// 256 random bits, drawn once per process. A token is
/// `HMAC-SHA256(key, collection || binding || position)`, which gives the
/// two properties this table needs at once: the same page yields the same
/// token, so a client polling one page does not mint a row per poll; and no
/// caller can compute or guess a token for a position it was not given,
/// because the key never leaves this process.
///
/// Per process rather than persisted: a restart simply starts deriving
/// under a new key, and the cursors handed out before it keep resolving,
/// because resolution matches the digest of the token the client holds and
/// not the key it was derived under. Nothing is invalidated and nothing is
/// stored that would have to be protected.
static CURSOR_TOKEN_KEY: LazyLock<[u8; TOKEN_BYTES]> = LazyLock::new(|| {
    let mut key = [0_u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
});

fn derive_token(collection: &str, binding_sha256: &str, position_json: &str) -> [u8; TOKEN_BYTES] {
    let mut mac = Hmac::<Sha256>::new_from_slice(CURSOR_TOKEN_KEY.as_slice())
        .expect("HMAC-SHA256 accepts a key of any length");
    mac.update(TOKEN_DOMAIN.as_bytes());
    mac.update(b"\n");
    mac.update(collection.as_bytes());
    mac.update(b"\n");
    mac.update(binding_sha256.as_bytes());
    mac.update(b"\n");
    mac.update(position_json.as_bytes());
    mac.finalize().into_bytes().into()
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

    /// Persist the next keyset position and return the URL-safe token that
    /// resumes from it. One slice of expired cursors is opportunistically
    /// removed in the same transaction, bounded by [`EXPIRY_BATCH`] so that
    /// a request never pays for a backlog the sweep has not reached yet.
    ///
    /// The same page of the same query issues the same token, and therefore
    /// reuses its row. That matters because a cursor is minted by *reading*:
    /// every answer that has a further page carries one, and the console
    /// polls the events and commands lists every few seconds without ever
    /// continuing the traversal. With a fresh random token per answer, one
    /// idle browser tab turned two reads into a durable write every three
    /// seconds - about 138,000 rows over a cursor's three-day life, each one
    /// committed on the process's single connection - and
    /// [`PageCursorStatistics::live`] counted them all as traversals a
    /// client was holding open. Deriving the token instead means a poll that
    /// asks the same question again gets the same answer and writes nothing.
    pub async fn issue(
        &self,
        collection: &str,
        query_binding: &str,
        position: PagePosition,
    ) -> CatalogResult<String> {
        validate_collection(collection)?;
        validate_position(&position)?;
        let binding_sha256 = binding_sha256(collection, query_binding);
        let position_json = serde_json::to_string(&position).map_err(|error| {
            CatalogError::invalid(format!("cannot encode pagination position: {error}"))
        })?;
        let raw_token = derive_token(collection, &binding_sha256, &position_json);
        let token = URL_SAFE_NO_PAD.encode(raw_token);
        debug_assert_eq!(token.len(), TOKEN_TEXT_BYTES);
        let token_sha256 = sha256_hex(&raw_token);
        let now = now_secs();
        let expires_at = now.saturating_add(CURSOR_TTL_SECONDS);
        let collection = collection.to_string();
        self.db
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                delete_expired_cursor_batch(&transaction, now)?;
                let held: Option<i64> = transaction
                    .query_row(
                        "SELECT expires_at FROM api_page_cursors WHERE token_sha256 = ?1",
                        [&token_sha256],
                        |row| row.get(0),
                    )
                    .optional()?;
                match held {
                    // A row this token can still be resumed from for most of
                    // a cursor's life: the caller is handed it again and
                    // nothing is written.
                    Some(held_until)
                        if held_until.saturating_sub(now) >= CURSOR_TTL_SECONDS / 2 =>
                    {
                        debug!(
                            collection,
                            "list cursor reissued from the row it was first issued from"
                        );
                    }
                    // Either nothing is stored for this page or what is
                    // stored is close enough to expiry that a client given it
                    // now could lose it mid-traversal. The row is replaced
                    // rather than updated because the table refuses updates
                    // on purpose; the token is a fingerprint of the position,
                    // so a replacement cannot move a cursor somewhere else.
                    held => {
                        if held.is_some() {
                            transaction.execute(
                                "DELETE FROM api_page_cursors WHERE token_sha256 = ?1",
                                [&token_sha256],
                            )?;
                        }
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
                    }
                }
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
    ///
    /// A backlog comes out [`EXPIRY_BATCH`] rows at a time, and the
    /// connection is taken again for each slice rather than held across all
    /// of them, so a table that filled up while this process was stopped is
    /// drained without any one statement blocking the requests that share
    /// the connection.
    pub async fn expire(&self, now: i64) -> CatalogResult<usize> {
        let mut removed = 0_usize;
        loop {
            let batch = self
                .db
                .with_connection(move |connection| {
                    Ok::<_, CatalogError>(delete_expired_cursor_batch(connection, now)?)
                })
                .await?;
            removed = removed.saturating_add(batch);
            if batch < EXPIRY_BATCH {
                break;
            }
        }
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

    /// A cursor is minted by reading, and the console re-reads the same
    /// first page of events and commands every few seconds without ever
    /// continuing the traversal. Asking the same question again therefore
    /// has to write nothing: the token is derived from the page it resumes,
    /// so the second answer is the first answer and the table does not grow
    /// a row per poll.
    #[tokio::test]
    async fn re_reading_the_same_page_reuses_its_cursor_row() {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let store = PageCursorStore::new(database.clone());
        let position = PagePosition {
            sort_value: 17,
            identity: "0199b603-20bd-7342-9be3-cef10620d8bb".to_string(),
            secondary: None,
        };
        let count = || {
            let database = Arc::clone(&database);
            async move {
                database
                    .with_connection(|connection| {
                        let count = connection.query_row(
                            "SELECT COUNT(*) FROM api_page_cursors",
                            [],
                            |row| row.get::<_, i64>(0),
                        )?;
                        Ok::<_, CatalogError>(count)
                    })
                    .await
                    .unwrap()
            }
        };

        let first = store
            .issue("events", "scope=all;sort=v1", position.clone())
            .await
            .unwrap();
        for _ in 0..20 {
            let again = store
                .issue("events", "scope=all;sort=v1", position.clone())
                .await
                .unwrap();
            assert_eq!(again, first, "the same page issued a different cursor");
        }
        assert_eq!(
            count().await,
            1,
            "one page, one row, whatever the poll rate"
        );
        assert_eq!(
            store
                .resolve(&first, "events", "scope=all;sort=v1")
                .await
                .unwrap(),
            position
        );

        // A different page, a different filter and a different collection
        // are each their own traversal and their own row.
        let further = PagePosition {
            sort_value: 18,
            identity: "0199b603-20bd-7342-9be3-cef10620d8bc".to_string(),
            secondary: None,
        };
        store
            .issue("events", "scope=all;sort=v1", further)
            .await
            .unwrap();
        store
            .issue("events", "scope=node-a;sort=v1", position.clone())
            .await
            .unwrap();
        store
            .issue("commands", "scope=all;sort=v1", position)
            .await
            .unwrap();
        assert_eq!(count().await, 4);

        let statistics = store.statistics(now_secs()).await.unwrap();
        assert_eq!(statistics.live, 4);
        assert_eq!(statistics.expired_pending_removal, 0);
    }

    /// A row that is close to expiring is replaced rather than handed out
    /// again, so a client that receives a cursor always has most of its life
    /// left in front of it.
    #[tokio::test]
    async fn a_cursor_close_to_expiry_is_reissued_with_a_full_life() {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let store = PageCursorStore::new(database.clone());
        let position = PagePosition {
            sort_value: 3,
            identity: "0199b603-20bd-7342-9be3-cef10620d8bb".to_string(),
            secondary: None,
        };
        let token = store
            .issue("events", "scope=all;sort=v1", position.clone())
            .await
            .unwrap();
        let token_sha256 = sha256_hex(&decode_token(&token).unwrap());
        let binding = binding_sha256("events", "scope=all;sort=v1");
        let position_json = serde_json::to_string(&position).unwrap();
        let nearly_gone = now_secs() + 60;
        database
            .with_connection(move |connection| {
                // The table refuses ordinary updates, so the row is rewritten.
                connection.execute(
                    "DELETE FROM api_page_cursors WHERE token_sha256 = ?1",
                    [&token_sha256],
                )?;
                connection.execute(
                    "INSERT INTO api_page_cursors
                       (token_sha256, collection, query_sha256, position_json,
                        created_at, expires_at)
                     SELECT ?1, 'events', ?2, ?3, ?4, ?5",
                    params![
                        token_sha256,
                        binding,
                        position_json,
                        nearly_gone - CURSOR_TTL_SECONDS,
                        nearly_gone
                    ],
                )?;
                Ok::<_, CatalogError>(())
            })
            .await
            .unwrap();

        let again = store
            .issue("events", "scope=all;sort=v1", position.clone())
            .await
            .unwrap();
        assert_eq!(again, token, "the token names the page, not the row");
        let statistics = store.statistics(now_secs()).await.unwrap();
        assert_eq!(statistics.live, 1, "still one row for one page");
        assert_eq!(
            statistics.oldest_live_age_seconds,
            Some(0),
            "the row was written again, so its life starts over"
        );
        assert_eq!(
            store
                .resolve(&again, "events", "scope=all;sort=v1")
                .await
                .unwrap(),
            position
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

    /// A backlog deeper than one slice still comes out in full, and no
    /// single statement removes more than a slice: the sweep is the only
    /// thing that bounds this table, it runs on the connection every
    /// request shares, and the pass it exists for is the one after a table
    /// filled up and then went quiet. Draining it in one statement would
    /// make the first request of the day wait for the whole backlog.
    #[tokio::test]
    async fn a_backlog_deeper_than_one_slice_is_drained_a_slice_at_a_time() {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let store = PageCursorStore::new(Arc::clone(&database));
        let now = 3_000_000;
        let expired = EXPIRY_BATCH * 2 + 7;
        database
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let mut insert = transaction.prepare(
                    "INSERT INTO api_page_cursors
                       (token_sha256, collection, query_sha256, position_json,
                        created_at, expires_at)
                     VALUES (?1, 'accounts', ?2, '{\"sortValue\":1,\"identity\":\"one\"}',
                             ?3, ?4)",
                )?;
                for index in 0..expired {
                    insert.execute(params![
                        format!("{index:064x}"),
                        "b".repeat(64),
                        now - 300_000,
                        now - 40_000 + index as i64,
                    ])?;
                }
                // One live row, so the sweep has a stopping point that is
                // not the end of the table.
                insert.execute(params![
                    "f".repeat(64),
                    "b".repeat(64),
                    now - 10,
                    now + 259_190,
                ])?;
                drop(insert);
                transaction.commit()?;
                Ok::<_, crate::database::DatabaseError>(())
            })
            .await
            .unwrap();

        // One statement takes a slice and stops.
        let first_slice = database
            .with_connection(move |connection| {
                Ok::<_, CatalogError>(delete_expired_cursor_batch(connection, now)?)
            })
            .await
            .unwrap();
        assert_eq!(first_slice, EXPIRY_BATCH);

        // The sweep keeps taking slices until one comes back short.
        assert_eq!(store.expire(now).await.unwrap(), expired - EXPIRY_BATCH);
        assert_eq!(store.expire(now).await.unwrap(), 0);
        assert_eq!(
            store.statistics(now).await.unwrap(),
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
                    EXPIRY_BATCH_SQL,
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
