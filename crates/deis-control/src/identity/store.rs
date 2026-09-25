//! Where accounts and tokens live, and how a presented credential becomes a
//! [`Principal`].
//!
//! Shares the one SQLite connection owned by [`crate::database::Database`],
//! like the catalog next door, so identity lookups never fight the catalog
//! for the write lock.
//!
//! Two rules shape the table layout:
//!
//! * **A row is not a credential.** A token stores its public id and
//!   `SHA-256` of its secret half. So a stolen copy of `deis-control.db`
//!   authenticates as nobody.
//! * **Revocation is a read, not a broadcast.** Every request looks the token
//!   row up, so a revoked token stops working on its next call. There is no
//!   cache to invalidate and no window to explain.

use std::sync::Arc;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

use super::token;
use super::{Authority, Principal, PrincipalKind, Role, Scope};
use crate::catalog::model::now_secs;
use crate::catalog::pagination::{KeysetDirection, KeysetResume, finish_page, sqlite_fetch_limit};
use crate::catalog::{CatalogError, CatalogPage, PageCursorStore, PagePosition};
use crate::database::{Database, DatabaseError};
use utoipa::ToSchema;

/// Longest an account or token name may be. Both are printed in audit lines
/// and in CLI tables, so they are short on purpose.
const MAX_NAME_LEN: usize = 64;

/// How stale `last_used_at` is allowed to be.
///
/// Without this every authenticated request would turn into a database
/// *write*, and this process has one connection: a busy control plane would
/// serialise every read behind a timestamp update nobody reads to the second.
/// A minute is far finer than the question this column answers, which is
/// "is anyone still using this token, or can I revoke it?".
const LAST_USED_RESOLUTION_SECS: i64 = 60;

const ACCOUNT_COLUMNS: &str = "id, name, role, created_at, disabled_at";
pub(crate) const TOKEN_COLUMNS: &str = "id, account_id, name, origin, scopes, unscoped, created_at, \
                             last_used_at, revoked_at";

/// Everything this store can refuse to do. The variants map one-to-one onto
/// HTTP status classes so the REST layer never has to guess.
#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("{0} not found")]
    NotFound(String),

    #[error("{0}")]
    Conflict(String),

    #[error("{0}")]
    Invalid(String),

    #[error("{0}")]
    Database(#[from] rusqlite::Error),

    /// The blocking database task did not finish - it panicked, or the
    /// runtime went away under it. Distinct from `Database` because there is
    /// no SQLite answer to report, only that the query did not happen.
    #[error("{0}")]
    Backend(String),
}

impl From<DatabaseError> for IdentityError {
    fn from(err: DatabaseError) -> Self {
        match err {
            DatabaseError::SqliteError(e) => IdentityError::Database(e),
            other => IdentityError::Backend(other.to_string()),
        }
    }
}

/// The shared pagination helpers speak the catalog's error type; the list
/// pages here map it onto the same HTTP classes every other identity
/// failure uses.
impl From<CatalogError> for IdentityError {
    fn from(err: CatalogError) -> Self {
        match err {
            CatalogError::NotFound(what) => IdentityError::NotFound(what),
            CatalogError::Invalid(what) => IdentityError::Invalid(what),
            CatalogError::Database(e) => IdentityError::Database(e),
            other => IdentityError::Backend(other.to_string()),
        }
    }
}

pub type IdentityResult<T> = Result<T, IdentityError>;

/// One account: the thing an audit line blames.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub created_at: i64,
    /// When an owner switched this account off. A disabled account's tokens
    /// are refused without being revoked, so switching it back on restores
    /// exactly the credentials that were there.
    pub disabled_at: Option<i64>,
}

/// Where a credential came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TokenOrigin {
    /// Minted by this control plane, shown once, stored as a hash.
    Minted,
    /// The one-time bootstrap credential minted for a control plane that had
    /// no credential at all. It may only create the first owner account and a
    /// first token for it; it is revoked after its first successful use and
    /// expires 24 hours after it was minted if unused.
    Bootstrap,
}

impl TokenOrigin {
    fn as_str(self) -> &'static str {
        match self {
            TokenOrigin::Minted => "minted",
            TokenOrigin::Bootstrap => "bootstrap",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "bootstrap" => TokenOrigin::Bootstrap,
            _ => TokenOrigin::Minted,
        }
    }
}

/// One credential, as a listing shows it. There is deliberately no field
/// that could hold the secret: this type is what `GET /api/tokens` serialises.
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct ApiToken {
    pub id: String,
    pub account_id: String,
    pub account: String,
    pub name: String,
    pub origin: TokenOrigin,
    /// The scope list as it is written, or `unscoped` when there is none.
    pub scopes: String,
    /// True when this credential is subject to no scope at all - the old flat
    /// key. Served as its own field so a dashboard or a script does not have
    /// to recognise the word.
    pub unscoped: bool,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

/// What resolving a presented credential produced.
///
/// The failures are separate because they are separate lines in the
/// log: an operator looking at "a revoked token was presented" knows a
/// credential is still deployed somewhere, which "no such token" does not
/// tell them. The *caller* is told the same thing in every case.
#[derive(Debug, Clone)]
pub enum Resolution {
    Allowed(Principal),
    /// No token with that id, or the secret did not match.
    Unknown,
    /// The token exists and was revoked.
    Revoked {
        token: String,
        account: String,
    },
    /// The token exists, is not revoked, and stayed past its expiry: an
    /// expired token is reported like a revoked one, because both mean a
    /// credential that was withdrawn is still out there.
    Expired {
        token: String,
        account: String,
    },
    /// The token is fine and its account is switched off.
    AccountDisabled {
        token: String,
        account: String,
    },
}

/// Which credentials a token listing is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokenScopeFilter {
    /// Every credential the other filters admit.
    #[default]
    All,
    /// Only the credentials that answer to no scope at all - the number an
    /// operator drives to zero. Combined with `include_revoked = false`
    /// this is exactly the count [`IdentityStore::unscoped_count`] reports.
    Unscoped,
}

impl TokenScopeFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenScopeFilter::All => "all",
            TokenScopeFilter::Unscoped => "unscoped",
        }
    }
}

/// The filters a token listing combines with "and".
#[derive(Debug, Clone, Default)]
pub struct TokenListFilter {
    /// The canonical account id, never its name: a listing bound to an id
    /// keeps meaning the same account whatever the name is later used for.
    pub account_id: Option<String>,
    /// Keep the credentials that no longer work. Off by default so a
    /// listing answers "what can reach this platform right now".
    pub include_revoked: bool,
    pub scope: TokenScopeFilter,
}

pub struct IdentityStore {
    db: Arc<Database>,
    /// Where "now" comes from. `now_secs` in production; a fixed or shifted
    /// clock in tests, which is how the 24-hour bootstrap expiry is verified
    /// without waiting for it.
    clock: Clock,
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

impl IdentityStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self::with_clock(db, Arc::new(now_secs))
    }

    /// The same store over an injected clock. Test-only in practice, kept
    /// `pub` so the binary's own wiring cannot silently grow a second
    /// definition of "now".
    pub fn with_clock(db: Arc<Database>, clock: Clock) -> Self {
        Self { db, clock }
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    /// The database connection and the clock, for a closure that needs both.
    /// The clock is read *before* the connection is entered, so a call cannot
    /// observe two different "now"s in one resolution.
    async fn with_conn_now<T, F>(&self, f: F) -> IdentityResult<T>
    where
        F: FnOnce(&Connection, i64) -> IdentityResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let now = self.now();
        self.with_conn(move |conn| f(conn, now)).await
    }

    /// Opaque list-cursor persistence for the account and token lists,
    /// backed by the same table the catalog lists use.
    pub fn page_cursors(&self) -> PageCursorStore {
        PageCursorStore::new(Arc::clone(&self.db))
    }

    async fn with_conn<T, F>(&self, f: F) -> IdentityResult<T>
    where
        F: FnOnce(&Connection) -> IdentityResult<T> + Send + 'static,
        T: Send + 'static,
    {
        self.db.with_connection(f).await
    }

    // ----------------------------------------------------------------
    // Accounts
    // ----------------------------------------------------------------

    pub async fn create_account(&self, name: &str, role: Role) -> IdentityResult<Account> {
        validate_name(name, "account name")?;
        let name = name.to_string();
        let created_at = now_secs();
        self.with_conn(move |conn| {
            if account_by_name(conn, &name)?.is_some() {
                return Err(IdentityError::Conflict(format!(
                    "an account named {name} already exists"
                )));
            }
            let id = Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO accounts (id, name, role, created_at, disabled_at) \
                 VALUES (?1, ?2, ?3, ?4, NULL)",
                params![id, name, role.as_str(), created_at],
            )?;
            info!(
                account = %name,
                role = %role,
                "Created an account; every token minted for it is capped by this role, so changing the role later narrows or widens credentials that already exist"
            );
            Ok(Account {
                id,
                name,
                role,
                created_at,
                disabled_at: None,
            })
        })
        .await
    }

    /// One page of accounts, oldest first, in the order `(created_at ASC,
    /// name ASC)`. The name is the tie-breaker because it is unique and
    /// never changes, so a cursor resumes at exactly one row. The total is
    /// counted in the same read transaction as the rows.
    pub async fn list_accounts_page(
        &self,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> IdentityResult<CatalogPage<Account>> {
        let fetch_limit = sqlite_fetch_limit(limit, "accounts")?;
        let resume = KeysetResume::value_and_identity(
            1,
            KeysetDirection::Ascending,
            "created_at",
            "name",
            cursor,
        );
        self.with_conn(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            let total = transaction.query_row("SELECT COUNT(*) FROM accounts", [], |row| {
                row.get::<_, i64>(0)
            })?;
            let mut stmt = transaction.prepare(&accounts_page_sql(&resume))?;
            let rows = stmt
                .query_map(
                    params_from_iter(resume.bindings([], fetch_limit)),
                    read_account,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            transaction.commit()?;
            Ok(finish_page(rows, limit, total, "accounts")?)
        })
        .await
    }

    pub async fn get_account(&self, name: &str) -> IdentityResult<Account> {
        let wanted = name.to_string();
        self.with_conn(move |conn| {
            account_by_name(conn, &wanted)?
                .ok_or_else(|| IdentityError::NotFound(format!("account {wanted}")))
        })
        .await
    }

    /// Change an account's role, its disabled flag, or both.
    ///
    /// Returns the account as it now is, so the caller can log what actually
    /// changed rather than what was asked for.
    pub async fn update_account(
        &self,
        name: &str,
        role: Option<Role>,
        disabled: Option<bool>,
    ) -> IdentityResult<Account> {
        let wanted = name.to_string();
        self.with_conn(move |conn| {
            let Some(account) = account_by_name(conn, &wanted)? else {
                return Err(IdentityError::NotFound(format!("account {wanted}")));
            };
            let role = role.unwrap_or(account.role);
            let disabled_at = match disabled {
                Some(true) => Some(account.disabled_at.unwrap_or_else(now_secs)),
                Some(false) => None,
                None => account.disabled_at,
            };
            conn.execute(
                "UPDATE accounts SET role = ?1, disabled_at = ?2 WHERE id = ?3",
                params![role.as_str(), disabled_at, account.id],
            )?;
            if role != account.role {
                warn!(
                    account = %account.name,
                    from = %account.role,
                    to = %role,
                    "Changed an account's role; the new ceiling applies to every token this account already holds, at their next request"
                );
            }
            if disabled_at.is_some() != account.disabled_at.is_some() {
                warn!(
                    account = %account.name,
                    disabled = disabled_at.is_some(),
                    "Switched an account; while it is off every one of its tokens is refused, and switching it back on restores exactly the same credentials"
                );
            }
            Ok(Account {
                role,
                disabled_at,
                ..account
            })
        })
        .await
    }

    // ----------------------------------------------------------------
    // Tokens
    // ----------------------------------------------------------------

    /// Mint a token for an account and hand back the one and only copy of
    /// its secret.
    ///
    /// The scopes are checked against the account's role here as well as on
    /// every request: a token that could never be used is a trap, and the
    /// person minting it is the one who can still fix the role.
    pub async fn create_token(
        &self,
        account_name: &str,
        token_name: &str,
        authority: Authority,
    ) -> IdentityResult<(ApiToken, String)> {
        validate_name(token_name, "token name")?;
        let account_name = account_name.to_string();
        let token_name = token_name.to_string();
        let created_at = now_secs();

        self.with_conn(move |conn| {
            let Some(account) = account_by_name(conn, &account_name)? else {
                return Err(IdentityError::NotFound(format!("account {account_name}")));
            };
            if let Authority::Scoped(scopes) = &authority {
                for scope in scopes {
                    if !account.role.permits(scope.verb) {
                        return Err(IdentityError::Invalid(format!(
                            "account {} has role {}, which cannot hold {scope}; give the account \
                             a role that can, or narrow the token",
                            account.name, account.role
                        )));
                    }
                }
            }

            let minted = token::mint();
            let (scopes_text, unscoped) = match &authority {
                Authority::Unscoped => (String::new(), true),
                Authority::Scoped(scopes) => (Scope::join(scopes), false),
            };
            conn.execute(
                "INSERT INTO api_tokens \
                 (id, account_id, name, origin, secret_hash, scopes, unscoped, created_at, \
                  last_used_at, revoked_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL)",
                params![
                    minted.id,
                    account.id,
                    token_name,
                    TokenOrigin::Minted.as_str(),
                    minted.secret_hash,
                    scopes_text,
                    unscoped as i64,
                    created_at
                ],
            )?;

            if unscoped {
                warn!(
                    account = %account.name,
                    token = %minted.id,
                    name = %token_name,
                    "Minted an UNSCOPED token: it can do anything its account's role allows, to every application. Prefer a scoped token and retire this one"
                );
            } else {
                info!(
                    account = %account.name,
                    token = %minted.id,
                    name = %token_name,
                    scopes = %scopes_text,
                    "Minted a token; the secret was shown to its creator once and is stored here only as a SHA-256, so it cannot be read back or recovered"
                );
            }

            Ok((
                ApiToken {
                    id: minted.id,
                    account_id: account.id,
                    account: account.name,
                    name: token_name,
                    origin: TokenOrigin::Minted,
                    scopes: scopes_text,
                    unscoped,
                    created_at,
                    last_used_at: None,
                    revoked_at: None,
                },
                minted.secret,
            ))
        })
        .await
    }

    /// One page of credentials, oldest first, in the order `(created_at
    /// ASC, id ASC)`; the token id is the tie-breaker. Every filter in
    /// `filter` narrows both the rows and the total, which is counted in
    /// the same read transaction as the rows.
    pub async fn list_tokens_page(
        &self,
        filter: TokenListFilter,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> IdentityResult<CatalogPage<ApiToken>> {
        let fetch_limit = sqlite_fetch_limit(limit, "tokens")?;
        let unscoped_only = filter.scope == TokenScopeFilter::Unscoped;
        let columns = TOKEN_COLUMNS.replace(", ", ", t.");
        // The cursor's parameters follow the filters, which is one more
        // parameter when the page is narrowed to an account.
        let first_cursor_param = if filter.account_id.is_some() { 4 } else { 3 };
        let resume = KeysetResume::value_and_identity(
            first_cursor_param,
            KeysetDirection::Ascending,
            "t.created_at",
            "t.id",
            cursor,
        );
        self.with_conn(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            // One statement per account filter. Written as `(?1 IS NULL OR
            // t.account_id = ?1)` the account is not an equality the planner
            // can seek, so a per-account page would read every token on the
            // platform in sort order; as a plain equality it seeks
            // `idx_api_tokens_account_page_v1`, while the whole collection
            // walks `idx_api_tokens_page_v1`. The revoked and unscoped
            // filters are applied on top of the ordered walk either way.
            // `pagination_plans` explains both statements.
            let (rows, total) = match &filter.account_id {
                Some(account_id) => {
                    let total = transaction.query_row(
                        "SELECT COUNT(*) FROM api_tokens t
                          WHERE t.account_id = ?1
                            AND (?2 = 1 OR t.revoked_at IS NULL)
                            AND (?3 = 0 OR t.unscoped = 1)",
                        params![account_id, filter.include_revoked, unscoped_only],
                        |row| row.get::<_, i64>(0),
                    )?;
                    let mut stmt =
                        transaction.prepare(&tokens_by_account_page_sql(&columns, &resume))?;
                    let rows = stmt
                        .query_map(
                            params_from_iter(resume.bindings(
                                [
                                    Value::from(account_id.clone()),
                                    Value::from(filter.include_revoked),
                                    Value::from(unscoped_only),
                                ],
                                fetch_limit,
                            )),
                            read_token,
                        )?
                        .collect::<Result<Vec<_>, _>>()?;
                    (rows, total)
                }
                None => {
                    let total = transaction.query_row(
                        "SELECT COUNT(*) FROM api_tokens t
                          WHERE (?1 = 1 OR t.revoked_at IS NULL)
                            AND (?2 = 0 OR t.unscoped = 1)",
                        params![filter.include_revoked, unscoped_only],
                        |row| row.get::<_, i64>(0),
                    )?;
                    let mut stmt = transaction.prepare(&tokens_page_sql(&columns, &resume))?;
                    let rows = stmt
                        .query_map(
                            params_from_iter(resume.bindings(
                                [
                                    Value::from(filter.include_revoked),
                                    Value::from(unscoped_only),
                                ],
                                fetch_limit,
                            )),
                            read_token,
                        )?
                        .collect::<Result<Vec<_>, _>>()?;
                    (rows, total)
                }
            };
            transaction.commit()?;
            Ok(finish_page(rows, limit, total, "tokens")?)
        })
        .await
    }

    /// Revoke one token. Answers the row as it now stands so the caller can
    /// say whose credential just stopped working.
    pub async fn revoke_token(&self, id: &str) -> IdentityResult<ApiToken> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let Some(token) = token_by_id(conn, &id)? else {
                return Err(IdentityError::NotFound(format!("token {id}")));
            };
            if token.revoked_at.is_some() {
                return Ok(token);
            }
            let revoked_at = now_secs();
            conn.execute(
                "UPDATE api_tokens SET revoked_at = ?1 WHERE id = ?2",
                params![revoked_at, id],
            )?;
            warn!(
                account = %token.account,
                token = %id,
                name = %token.name,
                "Revoked a token; it is refused from its very next request, and anything still \
                 presenting it - a pipeline, a script - now fails with 401 until it is given a new one"
            );
            Ok(ApiToken {
                revoked_at: Some(revoked_at),
                ..token
            })
        })
        .await
    }

    /// How many credentials answer to no scope at all.
    ///
    /// Reported at startup and by the listing, because the whole point of
    /// this model is that an operator can see which of their credentials are
    /// still root.
    pub async fn unscoped_count(&self) -> IdentityResult<usize> {
        self.with_conn(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM api_tokens WHERE unscoped = 1 AND revoked_at IS NULL",
                [],
                |row| row.get(0),
            )?;
            Ok(count as usize)
        })
        .await
    }

    // ----------------------------------------------------------------
    // Resolving a presented credential
    // ----------------------------------------------------------------

    /// Turn a presented `deis_pat_...` string into the caller it belongs to.
    ///
    /// One indexed lookup by the token's public id, then a constant-time
    /// comparison of the stored digest against the digest of what was
    /// presented. A wrong secret and an unknown id are the same answer to the
    /// caller and different lines in the log.
    pub async fn resolve_token(&self, presented: &str) -> IdentityResult<Resolution> {
        let Some(parts) = token::split(presented) else {
            return Ok(Resolution::Unknown);
        };
        let id = parts.id.to_string();
        let secret_half = parts.secret_half.to_string();

        self.with_conn_now(move |conn, now| {
            let row = conn
                .query_row(
                    "SELECT t.secret_hash, t.scopes, t.unscoped, t.revoked_at, \
                            t.last_used_at, a.name, a.role, a.disabled_at, t.origin, t.expires_at \
                     FROM api_tokens t JOIN accounts a ON a.id = t.account_id \
                     WHERE t.id = ?1",
                    params![id],
                    read_credential,
                )
                .optional()?;

            let Some(credential) = row else {
                return Ok(Resolution::Unknown);
            };

            // The secret is checked before anything else is reported, so a
            // caller who guesses an id learns nothing about whether it
            // exists, is revoked, or belongs to a disabled account.
            if !token::secret_matches(&credential.secret_hash, &secret_half) {
                return Ok(Resolution::Unknown);
            }
            if credential.revoked {
                return Ok(Resolution::Revoked {
                    token: id,
                    account: credential.account,
                });
            }
            if credential
                .expires_at
                .is_some_and(|expires_at| expires_at <= now)
            {
                return Ok(Resolution::Expired {
                    token: id,
                    account: credential.account,
                });
            }
            if credential.account_disabled {
                return Ok(Resolution::AccountDisabled {
                    token: id,
                    account: credential.account,
                });
            }

            touch_last_used(conn, &id, credential.last_used_at, now)?;

            Ok(Resolution::Allowed(Principal {
                kind: credential.kind,
                account: Some(credential.account),
                role: Some(credential.role),
                credential_id: Some(id),
                authority: authority_from_row(credential.unscoped, &credential.scopes),
            }))
        })
        .await
    }

    // ----------------------------------------------------------------
    // Bootstrap
    // ----------------------------------------------------------------

    /// Whether any live credential exists: a token that is not revoked and
    /// has not expired. This is the question a start-up asks before deciding
    /// to mint a bootstrap credential - one working credential anywhere means
    /// the platform is reachable and must not be handed a second root.
    pub async fn has_live_credential(&self) -> IdentityResult<bool> {
        self.with_conn_now(move |conn, now| {
            let live: i64 = conn.query_row(
                "SELECT COUNT(*) FROM api_tokens \
                 WHERE revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?1)",
                params![now],
                |row| row.get(0),
            )?;
            Ok(live > 0)
        })
        .await
    }

    /// Mint the one-time bootstrap credential, unless the platform already
    /// has a live credential.
    ///
    /// The case this exists for: a control plane with no accounts yet. Before
    /// accounts existed that state was a brick - every request refused, and
    /// no way in to create a way in. The bootstrap token can do one thing
    /// (create the first owner account and a first token for it; the gate in
    /// `crate::api::authz` enforces exactly that), is revoked after its first
    /// successful use, and expires after [`BOOTSTRAP_TTL_SECS`] if never
    /// used. The caller writes the secret to the bootstrap file; nothing here
    /// ever logs it.
    ///
    /// Returns the token's public id and its secret, in that order. The
    /// secret exists only in this return value and the file the caller
    /// writes; the row keeps the hash.
    pub async fn ensure_bootstrap_token(&self) -> IdentityResult<Option<(String, String)>> {
        if self.has_live_credential().await? {
            return Ok(None);
        }
        let now = self.now();
        let expires_at = now + BOOTSTRAP_TTL_SECS;
        self.with_conn(move |conn| ensure_account_row(conn, BOOTSTRAP_ACCOUNT, Role::Owner, now))
            .await?;
        let minted = token::mint();
        let row_id = minted.id.clone();
        let secret = minted.secret.clone();
        let secret_hash = minted.secret_hash;
        self.with_conn(move |conn| {
            let account = account_by_name(conn, BOOTSTRAP_ACCOUNT)?
                .expect("the bootstrap account was just ensured");
            conn.execute(
                "INSERT INTO api_tokens \
                 (id, account_id, name, origin, secret_hash, scopes, unscoped, created_at, \
                  last_used_at, revoked_at, expires_at) \
                 VALUES (?1, ?2, 'bootstrap', ?3, ?4, '', 0, ?5, NULL, NULL, ?6)",
                params![
                    row_id,
                    account.id,
                    TokenOrigin::Bootstrap.as_str(),
                    secret_hash,
                    now,
                    expires_at
                ],
            )?;
            Ok(())
        })
        .await?;
        Ok(Some((minted.id, secret)))
    }

    /// Install a preset bootstrap token: the operator's own secret, hashed
    /// and stored the same way a minted one is, expiring
    /// [`BOOTSTRAP_TTL_SECS`] from now.
    ///
    /// This is the headless path (`deis-control bootstrap --token-file`): the
    /// operator generates the secret offline, hands this binary the file it
    /// was written to, and logs in with the same file. Any previous bootstrap
    /// row stops working: there is one way in, and it is the one just
    /// installed.
    ///
    /// The secret must be a well-formed token string, because that is the
    /// shape `resolve_token` recognises. Refusing anything else here beats
    /// storing a credential that can never be presented.
    pub async fn install_preset_bootstrap_token(&self, secret: &str) -> IdentityResult<String> {
        let parts = token::split(secret).ok_or_else(|| {
            IdentityError::Invalid(
                "the bootstrap token is not a well-formed token string \
                 (deis_pat_<16 hex>_<64 hex>); keep that shape when generating one offline"
                    .to_string(),
            )
        })?;
        let id = parts.id.to_string();
        let secret_half = parts.secret_half.to_string();
        let secret_hash = token::hash_secret(&secret_half);
        let now = self.now();
        let expires_at = now + BOOTSTRAP_TTL_SECS;
        let row_id = id.clone();
        self.with_conn(move |conn| {
            let account_id = ensure_account_row(conn, BOOTSTRAP_ACCOUNT, Role::Owner, now)?;
            conn.execute(
                "UPDATE api_tokens SET revoked_at = ?2 \
                 WHERE origin = 'bootstrap' AND revoked_at IS NULL AND id != ?1",
                params![row_id, now],
            )?;
            conn.execute(
                "INSERT INTO api_tokens \
                 (id, account_id, name, origin, secret_hash, scopes, unscoped, created_at, \
                  last_used_at, revoked_at, expires_at) \
                 VALUES (?1, ?2, 'bootstrap', ?3, ?4, '', 0, ?5, NULL, NULL, ?6) \
                 ON CONFLICT(id) DO UPDATE SET \
                   account_id = ?2, secret_hash = ?4, revoked_at = NULL, expires_at = ?6",
                params![
                    row_id,
                    account_id,
                    TokenOrigin::Bootstrap.as_str(),
                    secret_hash,
                    now,
                    expires_at
                ],
            )?;
            Ok(())
        })
        .await?;
        Ok(id)
    }

    /// Revoke the bootstrap credential and say which id stopped working.
    /// Called by the gate right after the bootstrap token's first successful
    /// use, so the one-time credential is one-time even though nothing on the
    /// wire marks it as such.
    pub async fn revoke_bootstrap_token(&self, id: &str) -> IdentityResult<()> {
        let row_id = id.to_string();
        let now = self.now();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE api_tokens SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
                params![row_id, now],
            )?;
            Ok(())
        })
        .await?;
        info!(
            token = %id,
            "Revoked the bootstrap credential after its first successful use; the file that held it is deleted by the caller"
        );
        Ok(())
    }
}

/// The account a first-run control plane mints for itself, and the account
/// every bootstrap credential belongs to.
pub const BOOTSTRAP_ACCOUNT: &str = "bootstrap";

/// How long a bootstrap credential lives if it is never used. Short, because
/// the file holding it sits in the data directory; long enough for somebody
/// to notice a fresh install and log in.
pub const BOOTSTRAP_TTL_SECS: i64 = 24 * 60 * 60;

fn authority_from_row(unscoped: bool, scopes: &str) -> Authority {
    if unscoped {
        return Authority::Unscoped;
    }
    match Scope::parse_list(scopes) {
        Ok(scopes) => Authority::Scoped(scopes),
        Err(e) => {
            // A row this process cannot read is not a reason to admit its
            // holder to everything. The safe reading of an unparseable scope
            // list is the empty one, which grants nothing.
            warn!(
                error = %e,
                stored = %scopes,
                "A token's stored scopes could not be parsed, so it was treated as granting nothing; that token is refused everywhere until it is minted again"
            );
            Authority::Scoped(Vec::new())
        }
    }
}

/// Write `last_used_at` only when it would move by a useful amount. See
/// [`LAST_USED_RESOLUTION_SECS`] for why every request must not be a write.
fn touch_last_used(
    conn: &Connection,
    id: &str,
    last_used_at: Option<i64>,
    now: i64,
) -> IdentityResult<()> {
    let stale = match last_used_at {
        None => true,
        Some(previous) => now - previous >= LAST_USED_RESOLUTION_SECS,
    };
    if stale {
        conn.execute(
            "UPDATE api_tokens SET last_used_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
    }
    Ok(())
}

fn ensure_account_row(
    conn: &Connection,
    name: &str,
    role: Role,
    created_at: i64,
) -> IdentityResult<String> {
    if let Some(account) = account_by_name(conn, name)? {
        return Ok(account.id);
    }
    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO accounts (id, name, role, created_at, disabled_at) \
         VALUES (?1, ?2, ?3, ?4, NULL)",
        params![id, name, role.as_str(), created_at],
    )?;
    Ok(id)
}

fn account_by_name(conn: &Connection, name: &str) -> IdentityResult<Option<Account>> {
    let account = conn
        .query_row(
            &format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE name = ?1"),
            params![name],
            read_account,
        )
        .optional()?;
    Ok(account)
}

fn token_by_id(conn: &Connection, id: &str) -> IdentityResult<Option<ApiToken>> {
    let token = conn
        .query_row(
            &format!(
                "SELECT t.{}, a.name FROM api_tokens t JOIN accounts a ON a.id = t.account_id \
                 WHERE t.id = ?1",
                TOKEN_COLUMNS.replace(", ", ", t.")
            ),
            params![id],
            read_token,
        )
        .optional()?;
    Ok(token)
}

/// One token row joined to its account, as authenticating a request needs
/// it. A named shape rather than a tuple because the checks below read it
/// field by field and a tuple of nine would put the wrong index on one of
/// them exactly once.
struct Credential {
    secret_hash: String,
    scopes: String,
    unscoped: bool,
    revoked: bool,
    last_used_at: Option<i64>,
    account: String,
    role: Role,
    account_disabled: bool,
    kind: PrincipalKind,
    expires_at: Option<i64>,
}

fn read_credential(row: &Row<'_>) -> rusqlite::Result<Credential> {
    let role: String = row.get(6)?;
    let origin: String = row.get(8)?;
    Ok(Credential {
        secret_hash: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
        scopes: row.get(1)?,
        unscoped: row.get::<_, i64>(2)? != 0,
        revoked: row.get::<_, Option<i64>>(3)?.is_some(),
        last_used_at: row.get(4)?,
        account: row.get(5)?,
        // An unreadable role is the least privileged one, never the most.
        role: Role::parse(&role).unwrap_or(Role::Viewer),
        account_disabled: row.get::<_, Option<i64>>(7)?.is_some(),
        // An origin this build does not know is a minted token, which is
        // the ordinary case: only the bootstrap credential is special.
        kind: match TokenOrigin::parse(&origin) {
            TokenOrigin::Bootstrap => PrincipalKind::Bootstrap,
            TokenOrigin::Minted => PrincipalKind::Account,
        },
        expires_at: row.get(9)?,
    })
}

// --------------------------------------------------------------------
// Page statements
//
// The two forms a keyset page has: with the cursor predicate when the
// request resumes from one and without it otherwise, so SQLite seeks the
// page index instead of walking the collection (see
// `catalog::pagination::KeysetResume`). `catalog::pagination_plans`
// explains both forms of each against the real schema.
// --------------------------------------------------------------------

pub(crate) fn accounts_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT {ACCOUNT_COLUMNS} FROM accounts{resume_clause}
          ORDER BY created_at ASC, name ASC LIMIT ?{limit}",
        resume_clause = resume.where_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn tokens_page_sql(columns: &str, resume: &KeysetResume) -> String {
    format!(
        "SELECT t.{columns}, a.name FROM api_tokens t
           JOIN accounts a ON a.id = t.account_id
          WHERE (?1 = 1 OR t.revoked_at IS NULL)
            AND (?2 = 0 OR t.unscoped = 1){resume_clause}
          ORDER BY t.created_at ASC, t.id ASC LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn tokens_by_account_page_sql(columns: &str, resume: &KeysetResume) -> String {
    format!(
        "SELECT t.{columns}, a.name FROM api_tokens t
           JOIN accounts a ON a.id = t.account_id
          WHERE t.account_id = ?1
            AND (?2 = 1 OR t.revoked_at IS NULL)
            AND (?3 = 0 OR t.unscoped = 1){resume_clause}
          ORDER BY t.created_at ASC, t.id ASC LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

fn read_account(row: &Row<'_>) -> rusqlite::Result<Account> {
    let role: String = row.get(2)?;
    Ok(Account {
        id: row.get(0)?,
        name: row.get(1)?,
        // An unreadable role is the least privileged one, never the most:
        // a row written by a newer build must not become an owner here.
        role: Role::parse(&role).unwrap_or(Role::Viewer),
        created_at: row.get(3)?,
        disabled_at: row.get(4)?,
    })
}

fn read_token(row: &Row<'_>) -> rusqlite::Result<ApiToken> {
    let origin: String = row.get(3)?;
    let unscoped: i64 = row.get(5)?;
    let scopes: String = row.get(4)?;
    Ok(ApiToken {
        id: row.get(0)?,
        account_id: row.get(1)?,
        name: row.get(2)?,
        origin: TokenOrigin::parse(&origin),
        scopes: if unscoped != 0 {
            "unscoped".to_string()
        } else {
            scopes
        },
        unscoped: unscoped != 0,
        created_at: row.get(6)?,
        last_used_at: row.get(7)?,
        revoked_at: row.get(8)?,
        account: row.get(9)?,
    })
}

fn validate_name(value: &str, what: &str) -> IdentityResult<()> {
    if value.is_empty() {
        return Err(IdentityError::Invalid(format!("{what} may not be empty")));
    }
    if value.len() > MAX_NAME_LEN {
        return Err(IdentityError::Invalid(format!(
            "{what} is at most {MAX_NAME_LEN} characters; this one is {}",
            value.len()
        )));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '@')
    {
        return Err(IdentityError::Invalid(format!(
            "{what} {value:?} may only contain letters, digits, '-', '_', '.' and '@'; it is \
             printed in every audit line, so it stays plain"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Verb;
    use crate::identity::scope::{Demand, Selector};

    async fn new_store() -> IdentityStore {
        let db = Database::init(":memory:").await.unwrap();
        IdentityStore::new(Arc::new(db))
    }

    fn scoped(text: &str) -> Authority {
        Authority::Scoped(Scope::parse_list(text).unwrap())
    }

    /// The whole token list as one page, the way the tests below read it:
    /// `account` is a name here and is resolved to the id the filter takes.
    async fn tokens(
        store: &IdentityStore,
        account: Option<&str>,
        include_revoked: bool,
    ) -> Vec<ApiToken> {
        let account_id = match account {
            Some(name) => Some(store.get_account(name).await.unwrap().id),
            None => None,
        };
        store
            .list_tokens_page(
                TokenListFilter {
                    account_id,
                    include_revoked,
                    scope: TokenScopeFilter::All,
                },
                100,
                None,
            )
            .await
            .unwrap()
            .items
    }

    /// The whole life of a scoped credential: minted once, usable, listed
    /// without its secret, revoked, and then refused.
    #[tokio::test]
    async fn a_token_works_until_it_is_revoked_and_never_appears_in_a_listing() {
        let store = new_store().await;
        store.create_account("ci", Role::Deployer).await.unwrap();
        let (token, secret) = store
            .create_token("ci", "preview-pipeline", scoped("deploy:myapp/preview"))
            .await
            .unwrap();

        let Resolution::Allowed(principal) = store.resolve_token(&secret).await.unwrap() else {
            panic!("a freshly minted token has to authenticate");
        };
        assert_eq!(principal.account.as_deref(), Some("ci"));
        assert_eq!(principal.role, Some(Role::Deployer));
        assert!(
            principal
                .permits(&Demand {
                    verb: Verb::Deploy,
                    app: Selector::One("myapp"),
                    environment: Selector::One("preview"),
                })
                .is_ok()
        );

        let listed = tokens(&store, None, false).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].scopes, "deploy:myapp/preview");
        assert!(
            !serde_json::to_string(&listed).unwrap().contains(&secret),
            "a listing must not carry the credential"
        );

        store.revoke_token(&token.id).await.unwrap();
        assert!(
            matches!(
                store.resolve_token(&secret).await.unwrap(),
                Resolution::Revoked { .. }
            ),
            "a revoked token must be refused on its very next request"
        );
        assert!(tokens(&store, None, false).await.is_empty());
        assert_eq!(tokens(&store, None, true).await.len(), 1);
    }

    /// The point of storing a digest: whatever a reader of the database
    /// finds, it does not authenticate.
    #[tokio::test]
    async fn nothing_stored_about_a_token_can_be_presented_as_one() {
        let store = new_store().await;
        store.create_account("alice", Role::Owner).await.unwrap();
        let (token, secret) = store
            .create_token("alice", "laptop", Authority::Unscoped)
            .await
            .unwrap();

        let stored: Vec<String> = store
            .db
            .with_connection(|conn| {
                let mut stmt = conn.prepare("SELECT id, secret_hash, scopes FROM api_tokens")?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(format!(
                            "{} {} {}",
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                            row.get::<_, String>(2)?
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, DatabaseError>(rows)
            })
            .await
            .unwrap();
        let dump = stored.join("\n");
        assert!(dump.contains(&token.id), "the public half is stored");
        assert!(
            !dump.contains(&secret),
            "the database must not contain the credential"
        );
        for line in dump.split_whitespace() {
            assert!(
                matches!(
                    store.resolve_token(line).await.unwrap(),
                    Resolution::Unknown
                ),
                "{line} was accepted as a credential"
            );
        }
    }

    /// Disabling an account stops its tokens without revoking them, and
    /// enabling it again brings exactly those tokens back. That is the
    /// difference between a person on leave and a credential that leaked.
    #[tokio::test]
    async fn disabling_an_account_stops_its_tokens_and_enabling_it_restores_them() {
        let store = new_store().await;
        store.create_account("bob", Role::Maintainer).await.unwrap();
        let (_, secret) = store
            .create_token("bob", "laptop", scoped("read:*/*"))
            .await
            .unwrap();

        store.update_account("bob", None, Some(true)).await.unwrap();
        assert!(matches!(
            store.resolve_token(&secret).await.unwrap(),
            Resolution::AccountDisabled { .. }
        ));

        store
            .update_account("bob", None, Some(false))
            .await
            .unwrap();
        assert!(matches!(
            store.resolve_token(&secret).await.unwrap(),
            Resolution::Allowed(_)
        ));
    }

    /// A token may not be minted with more than its account's role can hold:
    /// it would authenticate and then be refused on every call, which is a
    /// trap for whoever was given it.
    #[tokio::test]
    async fn a_token_cannot_be_minted_beyond_its_accounts_role() {
        let store = new_store().await;
        store.create_account("ci", Role::Deployer).await.unwrap();
        let refused = store
            .create_token("ci", "too-much", scoped("secrets:myapp/production"))
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("role deployer"), "{refused}");
        assert!(refused.contains("secrets:myapp/production"), "{refused}");
    }

    /// The first-run case: no credential anywhere. Exactly one bootstrap
    /// credential is minted; it authenticates, it expires 24 hours after it
    /// was minted (verified with an injected clock), a restart mints no
    /// second one while it is live, and after it is revoked the platform
    /// mints a fresh one on the next start - which is how an operator who
    /// lost the file gets back in.
    #[tokio::test]
    async fn a_control_plane_with_no_credential_at_all_mints_one_expiring_bootstrap_token() {
        let store = new_store().await;
        let (id, secret) = store
            .ensure_bootstrap_token()
            .await
            .unwrap()
            .expect("a control plane with no credential has to mint one");

        let Resolution::Allowed(principal) = store.resolve_token(&secret).await.unwrap() else {
            panic!("a freshly minted bootstrap credential has to authenticate");
        };
        assert_eq!(principal.kind, PrincipalKind::Bootstrap);
        assert_eq!(principal.account.as_deref(), Some(BOOTSTRAP_ACCOUNT));
        assert_eq!(id.len(), 16, "the public half is eight bytes of hex");

        // A restart while the bootstrap credential is live mints nothing:
        // one way in at a time.
        assert!(
            store.ensure_bootstrap_token().await.unwrap().is_none(),
            "a restart must not mint a second root credential"
        );

        // Expired, it is refused and reported like the withdrawn credential
        // it now is. Two stores share one database and differ only in the
        // clock: the same preset credential is live to the real clock and
        // expired to one that has run past the TTL it was born with.
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let installer = Arc::new(IdentityStore::with_clock(
            Arc::clone(&database),
            Arc::new(now_secs),
        ));
        let shifted = Arc::new(IdentityStore::with_clock(
            Arc::clone(&database),
            Arc::new(|| now_secs() + BOOTSTRAP_TTL_SECS + 1),
        ));
        let preset = installer
            .install_preset_bootstrap_token(&secret)
            .await
            .unwrap();
        assert_eq!(preset, id, "the id comes from the token itself");
        assert!(matches!(
            installer.resolve_token(&secret).await.unwrap(),
            Resolution::Allowed(_)
        ));
        assert!(matches!(
            shifted.resolve_token(&secret).await.unwrap(),
            Resolution::Expired { .. }
        ));
    }

    /// A live account token stops the bootstrap mint, and once every
    /// credential is gone a fresh start mints a new bootstrap credential
    /// again - that is the recovery path for a lost bootstrap file.
    #[tokio::test]
    async fn a_live_token_prevents_a_bootstrap_mint_and_a_used_up_platform_gets_a_fresh_one() {
        let store = new_store().await;
        store.create_account("alice", Role::Owner).await.unwrap();
        let (token, secret) = store
            .create_token("alice", "laptop", Authority::Unscoped)
            .await
            .unwrap();
        assert!(
            store.ensure_bootstrap_token().await.unwrap().is_none(),
            "a platform with a live credential must not be handed a second root"
        );

        store.revoke_token(&token.id).await.unwrap();
        let (new_id, new_secret) = store
            .ensure_bootstrap_token()
            .await
            .unwrap()
            .expect("no live credential is left, so a way in is minted");
        assert!(matches!(
            store.resolve_token(&new_secret).await.unwrap(),
            Resolution::Allowed(_)
        ));
        assert_ne!(new_id, token.id);
        assert!(matches!(
            store.resolve_token(&secret).await.unwrap(),
            Resolution::Revoked { .. }
        ));
    }

    /// A preset bootstrap token replaces anything left over from an earlier
    /// attempt, and a string that is not a token shape is refused rather
    /// than stored as a credential nobody could ever present.
    #[tokio::test]
    async fn a_preset_bootstrap_token_replaces_the_previous_one_and_refuses_malformed_input() {
        let store = new_store().await;
        let (first_id, first_secret) = store.ensure_bootstrap_token().await.unwrap().expect("mint");

        let replacement = format!("deis_pat_ffffffffffffffff_{}", "a".repeat(64));
        let second_id = store
            .install_preset_bootstrap_token(&replacement)
            .await
            .unwrap();
        assert_ne!(second_id, first_id);
        assert!(matches!(
            store.resolve_token(&first_secret).await.unwrap(),
            Resolution::Revoked { .. }
        ));
        assert!(matches!(
            store.resolve_token(&replacement).await.unwrap(),
            Resolution::Allowed(p) if p.kind == PrincipalKind::Bootstrap
        ));

        let refused = store
            .install_preset_bootstrap_token("not-a-token")
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("well-formed"), "{refused}");
    }

    /// Revoking the bootstrap credential after its first use is the gate's
    /// job, but the store does the database half: after it, the credential
    /// reads as revoked and a second use of the same secret is refused.
    #[tokio::test]
    async fn a_bootstrap_token_revoked_after_use_is_refused_on_its_next_presentation() {
        let store = new_store().await;
        let (id, secret) = store.ensure_bootstrap_token().await.unwrap().expect("mint");
        store.revoke_bootstrap_token(&id).await.unwrap();
        assert!(matches!(
            store.resolve_token(&secret).await.unwrap(),
            Resolution::Revoked { .. }
        ));
    }

    /// `last_used_at` answers "can I revoke this?", so it has to be written
    /// at least once - a column that stayed NULL forever would be the second
    /// defect shape in this repository, a field nothing ever fills.
    #[tokio::test]
    async fn using_a_token_records_that_it_was_used() {
        let store = new_store().await;
        store.create_account("ci", Role::Viewer).await.unwrap();
        let (token, secret) = store
            .create_token("ci", "reader", scoped("read:*/*"))
            .await
            .unwrap();
        assert!(token.last_used_at.is_none());

        store.resolve_token(&secret).await.unwrap();
        let listed = tokens(&store, Some("ci"), false).await;
        assert!(
            listed[0].last_used_at.is_some(),
            "a token that authenticated must be recorded as used"
        );
    }
}
