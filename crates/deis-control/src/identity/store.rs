//! Where accounts and tokens live, and how a presented credential becomes a
//! [`Principal`].
//!
//! Shares the one SQLite connection owned by [`crate::database::Database`],
//! like the catalog next door, so identity lookups never fight the catalog
//! for the write lock.
//!
//! Two rules shape the table layout:
//!
//! * **A row is not a credential.** A minted token stores its public id and
//!   `SHA-256` of its secret half. A configured `api_keys` entry stores
//!   *nothing* about the key itself, not even a full digest - only the
//!   four-byte fingerprint that already appeared in the log, and it is
//!   checked in memory against the configuration file exactly as it always
//!   was. So a stolen copy of `deis-control.db` authenticates as nobody.
//! * **Revocation is a read, not a broadcast.** Every request looks the token
//!   row up, so a revoked token stops working on its next call. There is no
//!   cache to invalidate and no window to explain.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Row, params};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

use super::token;
use super::{Authority, Principal, PrincipalKind, Role, Scope};
use crate::catalog::model::now_secs;
use crate::database::{Database, DatabaseError};

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
const TOKEN_COLUMNS: &str = "id, account_id, name, origin, scopes, unscoped, created_at, \
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

pub type IdentityResult<T> = Result<T, IdentityError>;

/// One account: the thing an audit line blames.
#[derive(Debug, Clone, serde::Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenOrigin {
    /// Minted by this control plane, shown once, stored as a hash.
    Minted,
    /// An `api_keys` entry in the configuration file. The row exists so the
    /// key is visible in a listing and nameable in an audit line; the key
    /// itself is checked against the file and is not in this database.
    Config,
}

impl TokenOrigin {
    fn as_str(self) -> &'static str {
        match self {
            TokenOrigin::Minted => "minted",
            TokenOrigin::Config => "config",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "config" => TokenOrigin::Config,
            _ => TokenOrigin::Minted,
        }
    }
}

/// One credential, as a listing shows it. There is deliberately no field
/// that could hold the secret: this type is what `GET /api/tokens` serialises.
#[derive(Debug, Clone, serde::Serialize)]
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
/// The three failures are separate because they are separate lines in the
/// log: an operator looking at "a revoked token was presented" knows a
/// credential is still deployed somewhere, which "no such token" does not
/// tell them. The *caller* is told the same thing in all three cases.
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
    /// The token is fine and its account is switched off.
    AccountDisabled {
        token: String,
        account: String,
    },
}

pub struct IdentityStore {
    db: Arc<Database>,
}

impl IdentityStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
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

    pub async fn list_accounts(&self) -> IdentityResult<Vec<Account>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY created_at, name"
            ))?;
            let rows = stmt
                .query_map([], read_account)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
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
                  last_used_at, revoked_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL)",
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
                    "Minted an UNSCOPED token: it can do anything its account's role allows, to every application. This is the old flat api_keys behaviour; prefer a scoped token and retire this one"
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

    /// Every credential this control plane knows about, newest last.
    ///
    /// `account` narrows the list to one account's; `include_revoked` keeps
    /// the ones that no longer work, because "when did this stop working"
    /// is a question an operator asks after an incident.
    pub async fn list_tokens(
        &self,
        account: Option<&str>,
        include_revoked: bool,
    ) -> IdentityResult<Vec<ApiToken>> {
        let account = account.map(|a| a.to_string());
        self.with_conn(move |conn| {
            let mut sql = format!(
                "SELECT t.{}, a.name FROM api_tokens t JOIN accounts a ON a.id = t.account_id",
                TOKEN_COLUMNS.replace(", ", ", t.")
            );
            let mut clauses: Vec<String> = Vec::new();
            if account.is_some() {
                clauses.push("a.name = ?1".to_string());
            }
            if !include_revoked {
                clauses.push("t.revoked_at IS NULL".to_string());
            }
            if !clauses.is_empty() {
                sql.push_str(" WHERE ");
                sql.push_str(&clauses.join(" AND "));
            }
            sql.push_str(" ORDER BY t.created_at, t.id");

            let mut stmt = conn.prepare(&sql)?;
            let rows = match &account {
                Some(name) => stmt
                    .query_map(params![name], read_token)?
                    .collect::<Result<Vec<_>, _>>()?,
                None => stmt
                    .query_map([], read_token)?
                    .collect::<Result<Vec<_>, _>>()?,
            };
            Ok(rows)
        })
        .await
    }

    /// Revoke one token. Answers the row as it now stands so the caller can
    /// say whose credential just stopped working.
    ///
    /// A configured `api_keys` entry cannot be revoked here: it is not in
    /// this database, it is in the configuration file, and a row saying it is
    /// revoked while the file still names it would be a lie the next restart
    /// would undo. The refusal says which file to edit.
    pub async fn revoke_token(&self, id: &str) -> IdentityResult<ApiToken> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let Some(token) = token_by_id(conn, &id)? else {
                return Err(IdentityError::NotFound(format!("token {id}")));
            };
            if token.origin == TokenOrigin::Config {
                return Err(IdentityError::Invalid(format!(
                    "token {id} is an api_keys entry from this control plane's configuration \
                     file, not a minted token. Remove it from api_keys and restart deis-control; \
                     until then it keeps working, and a row here saying otherwise would be wrong"
                )));
            }
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
        let now = now_secs();

        self.with_conn(move |conn| {
            let row = conn
                .query_row(
                    "SELECT t.secret_hash, t.scopes, t.unscoped, t.revoked_at, \
                            t.last_used_at, a.name, a.role, a.disabled_at \
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
            if credential.account_disabled {
                return Ok(Resolution::AccountDisabled {
                    token: id,
                    account: credential.account,
                });
            }

            touch_last_used(conn, &id, credential.last_used_at, now)?;

            Ok(Resolution::Allowed(Principal {
                kind: PrincipalKind::Account,
                account: Some(credential.account),
                role: Some(credential.role),
                credential_id: Some(id),
                authority: authority_from_row(credential.unscoped, &credential.scopes),
            }))
        })
        .await
    }

    /// The caller behind an `api_keys` entry that the configuration file has
    /// already accepted.
    ///
    /// The key itself never reaches this function; only its fingerprint,
    /// which is the row's id. If the row is missing - a database wiped under
    /// a running control plane, say - the caller is still admitted as the
    /// unscoped credential the configuration says it is, because the
    /// configuration file is the authority for these and this table is only
    /// the record of them.
    pub async fn resolve_config_key(&self, fingerprint: &str) -> IdentityResult<Principal> {
        let id = fingerprint.to_string();
        let now = now_secs();
        self.with_conn(move |conn| {
            let row: Option<(String, Option<i64>)> = conn
                .query_row(
                    "SELECT a.name, t.last_used_at FROM api_tokens t \
                     JOIN accounts a ON a.id = t.account_id WHERE t.id = ?1",
                    params![id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let account = match row {
                Some((account, last_used_at)) => {
                    touch_last_used(conn, &id, last_used_at, now)?;
                    account
                }
                None => CONFIG_KEY_ACCOUNT.to_string(),
            };
            Ok(Principal {
                kind: PrincipalKind::ConfigKey,
                account: Some(account),
                role: Some(Role::Owner),
                credential_id: Some(id),
                authority: Authority::Unscoped,
            })
        })
        .await
    }

    // ----------------------------------------------------------------
    // Startup
    // ----------------------------------------------------------------

    /// Bring the record of the configured `api_keys` into line with the file,
    /// and report how many there are.
    ///
    /// Three things happen, all of them so that an operator can *see* their
    /// flat keys rather than only remember them: a row appears for every key
    /// the file names, keys the file no longer names are marked revoked (the
    /// file is the authority, so a key that left it has genuinely stopped
    /// working and the listing must say so), and the whole set hangs off one
    /// placeholder account whose role is `owner`, because that is what an
    /// `api_keys` entry has always been.
    pub async fn sync_config_keys(&self, keys: &[String]) -> IdentityResult<usize> {
        let fingerprints: Vec<String> = keys
            .iter()
            .filter(|key| !key.trim().is_empty())
            .map(|key| token::fingerprint(key))
            .collect();
        let present = fingerprints.len();
        if present == 0 {
            // Nothing in the file. Anything left over from a previous run has
            // stopped working, and the listing has to stop showing it as live.
            let revoked = self
                .with_conn(move |conn| revoke_missing_config_keys(conn, &[]))
                .await?;
            if revoked > 0 {
                warn!(
                    revoked,
                    "The configuration file no longer names any api_keys, so that many recorded configuration credentials were marked revoked; anything still presenting one now gets 401"
                );
            }
            return Ok(0);
        }

        let created_at = now_secs();
        let wanted = fingerprints.clone();
        self.with_conn(move |conn| {
            let account_id = ensure_account_row(conn, CONFIG_KEY_ACCOUNT, Role::Owner, created_at)?;
            for (index, fingerprint) in wanted.iter().enumerate() {
                conn.execute(
                    "INSERT INTO api_tokens \
                     (id, account_id, name, origin, secret_hash, scopes, unscoped, created_at, \
                      last_used_at, revoked_at) \
                     VALUES (?1, ?2, ?3, ?4, NULL, '', 1, ?5, NULL, NULL) \
                     ON CONFLICT(id) DO UPDATE SET revoked_at = NULL, account_id = ?2",
                    params![
                        fingerprint,
                        account_id,
                        format!("api_keys[{index}]"),
                        TokenOrigin::Config.as_str(),
                        created_at
                    ],
                )?;
            }
            let revoked = revoke_missing_config_keys(conn, &wanted)?;
            if revoked > 0 {
                warn!(
                    revoked,
                    "Configuration credentials that the file no longer names were marked revoked; anything still presenting one now gets 401"
                );
            }
            Ok(())
        })
        .await?;

        warn!(
            api_keys = present,
            account = CONFIG_KEY_ACCOUNT,
            "This control plane's api_keys are UNSCOPED credentials: each one may deploy any application, rotate any secret, mint any tenant's storage token and delete anything. They keep working exactly as before and are listed by `deis token list`; replace them with scoped tokens and then take them out of the configuration file"
        );
        Ok(present)
    }

    /// Make sure this control plane has at least one credential a person can
    /// use, and say so once when it had to invent one.
    ///
    /// The case this exists for: `api_auth_enabled = true` with an empty
    /// `api_keys` and no accounts yet. Before accounts existed that
    /// combination was a brick - every request refused, and no way in to
    /// create a way in. Now it mints an owner account with one unscoped
    /// token, printed once because it is stored only as a hash and cannot be
    /// read back.
    ///
    /// It deliberately does nothing when any usable credential already
    /// exists, so a restart never mints a second root token.
    pub async fn ensure_bootstrap_credential(
        &self,
        configured_keys: usize,
    ) -> IdentityResult<Option<String>> {
        if configured_keys > 0 {
            return Ok(None);
        }
        let live: i64 = self
            .with_conn(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM api_tokens WHERE revoked_at IS NULL",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await?;
        if live > 0 {
            return Ok(None);
        }

        let created_at = now_secs();
        self.with_conn(move |conn| {
            ensure_account_row(conn, BOOTSTRAP_ACCOUNT, Role::Owner, created_at)?;
            Ok(())
        })
        .await?;
        let (_, secret) = self
            .create_token(BOOTSTRAP_ACCOUNT, "bootstrap", Authority::Unscoped)
            .await?;
        Ok(Some(secret))
    }
}

/// The account every `api_keys` entry is filed under. A placeholder, not a
/// person: it exists so an audit line has a name to print and so the listing
/// can group them.
pub const CONFIG_KEY_ACCOUNT: &str = "config-key";

/// The account a first-run control plane mints for itself.
pub const BOOTSTRAP_ACCOUNT: &str = "bootstrap";

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

fn revoke_missing_config_keys(conn: &Connection, keep: &[String]) -> IdentityResult<usize> {
    let mut stmt =
        conn.prepare("SELECT id FROM api_tokens WHERE origin = 'config' AND revoked_at IS NULL")?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    let now = now_secs();
    let mut revoked = 0;
    for id in existing {
        if keep.contains(&id) {
            continue;
        }
        conn.execute(
            "UPDATE api_tokens SET revoked_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        revoked += 1;
    }
    Ok(revoked)
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
}

fn read_credential(row: &Row<'_>) -> rusqlite::Result<Credential> {
    let role: String = row.get(6)?;
    Ok(Credential {
        // A configured key has no digest here at all, and an empty one
        // matches no secret, so such a row can never authenticate through
        // this path - which is the intent: those are checked against the
        // configuration file.
        secret_hash: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
        scopes: row.get(1)?,
        unscoped: row.get::<_, i64>(2)? != 0,
        revoked: row.get::<_, Option<i64>>(3)?.is_some(),
        last_used_at: row.get(4)?,
        account: row.get(5)?,
        // An unreadable role is the least privileged one, never the most.
        role: Role::parse(&role).unwrap_or(Role::Viewer),
        account_disabled: row.get::<_, Option<i64>>(7)?.is_some(),
    })
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

        let listed = store.list_tokens(None, false).await.unwrap();
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
        assert!(store.list_tokens(None, false).await.unwrap().is_empty());
        assert_eq!(store.list_tokens(None, true).await.unwrap().len(), 1);
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

    /// A configured key is recorded so it can be seen, and the record holds
    /// nothing that could be presented. Taking the key out of the file
    /// revokes the record, because the file is the authority.
    #[tokio::test]
    async fn configured_keys_are_visible_unscoped_and_follow_the_file() {
        let store = new_store().await;
        let keys = vec!["first-config-key".to_string(), "second".to_string()];
        assert_eq!(store.sync_config_keys(&keys).await.unwrap(), 2);
        assert_eq!(store.unscoped_count().await.unwrap(), 2);

        let listed = store.list_tokens(None, false).await.unwrap();
        assert_eq!(listed.len(), 2);
        for entry in &listed {
            assert!(entry.unscoped);
            assert_eq!(entry.scopes, "unscoped");
            assert_eq!(entry.origin, TokenOrigin::Config);
            assert_eq!(entry.account, CONFIG_KEY_ACCOUNT);
        }

        // The key itself is checked against the file, not here; this is the
        // record of it, resolved by fingerprint.
        let fingerprint = token::fingerprint(&keys[0]);
        let principal = store.resolve_config_key(&fingerprint).await.unwrap();
        assert!(principal.authority.is_unscoped());
        assert_eq!(principal.kind, PrincipalKind::ConfigKey);

        // Nothing stored about it can be presented as a credential.
        assert!(matches!(
            store.resolve_token(&fingerprint).await.unwrap(),
            Resolution::Unknown
        ));

        // One key removed from the file: its record stops being live.
        assert_eq!(store.sync_config_keys(&keys[1..]).await.unwrap(), 1);
        assert_eq!(store.unscoped_count().await.unwrap(), 1);
        let revoked = store.revoke_token(&fingerprint).await.unwrap_err();
        assert!(revoked.to_string().contains("api_keys"), "{revoked}");
    }

    /// The first-run case: authentication demanded and no credential
    /// anywhere. One owner token is minted and printed; a restart mints no
    /// second one.
    #[tokio::test]
    async fn a_control_plane_with_no_credential_at_all_mints_exactly_one() {
        let store = new_store().await;
        let secret = store
            .ensure_bootstrap_credential(0)
            .await
            .unwrap()
            .expect("a control plane with no credential has to mint one");
        assert!(matches!(
            store.resolve_token(&secret).await.unwrap(),
            Resolution::Allowed(_)
        ));
        assert!(
            store
                .ensure_bootstrap_credential(0)
                .await
                .unwrap()
                .is_none(),
            "a restart must not mint a second root credential"
        );

        // And a control plane that has configured keys mints nothing.
        let other = new_store().await;
        assert!(
            other
                .ensure_bootstrap_credential(1)
                .await
                .unwrap()
                .is_none()
        );
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
        let listed = store.list_tokens(Some("ci"), false).await.unwrap();
        assert!(
            listed[0].last_used_at.is_some(),
            "a token that authenticated must be recorded as used"
        );
    }
}
