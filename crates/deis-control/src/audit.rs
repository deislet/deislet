//! The audit log: who changed what, on which application and environment,
//! from where, and how it ended.
//!
//! This is the half of the authorisation model that keeps working after
//! something has already gone wrong. Scopes decide what a credential may do;
//! this decides what an operator can find out afterwards, and it is the only
//! record that survives the credential being rotated, the account being
//! disabled or the person leaving.
//!
//! # What is written down, and what is not
//!
//! Every **state-changing** call is recorded whatever its outcome, and every
//! **refused** call is recorded whatever it was asking for. Successful reads
//! are not: they are the overwhelming majority of the traffic, and a table
//! that holds them is a request log in which the two lines that matter cannot
//! be found. The one read that is always recorded is the storage-token mint,
//! because what it hands back is a credential.
//!
//! What never reaches this table is a value. A secret write records the
//! secret's *name*, taken from the request path, and the recorder never reads
//! a request body except on the two routes named in [`crate::api::authz`],
//! neither of which carries a secret. `scripts/demo.sh verify` greps every
//! log and data directory for the demo secret, and this table is one of the
//! places it looks.
//!
//! A line also carries the request id the answer echoed in `x-request-id`,
//! when there was one, so the caller holding a response and the operator
//! reading the log are talking about the same call.
//!
//! # Why nothing can erase it
//!
//! There is no update and no delete anywhere in this module, and no endpoint
//! that reaches one - not for an owner either. The only way to remove a line
//! is to have the operating system's access to the database file, which is
//! also the level at which the secret master key and the bundle store live.
//! Retention is therefore unbounded; see the note on [`AuditStore::record`].

use std::sync::Arc;

use rusqlite::{Row, params};
use serde::Serialize;
use tracing::{error, info, warn};
use utoipa::ToSchema;

use crate::catalog::model::now_secs;
use crate::catalog::pagination::{finish_page, sqlite_fetch_limit};
use crate::catalog::{CatalogError, CatalogPage, CatalogResult, PageCursorStore, PagePosition};
use crate::database::{Database, DatabaseError};
use crate::identity::{Principal, PrincipalKind};

const AUDIT_COLUMNS: &str = "id, at, actor_kind, actor_account, actor_credential, action, \
                             app_id, environment, target, method, path, peer, outcome, \
                             status, detail, request_id";

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The caller was entitled to it and it worked.
    Accepted,
    /// The caller was not entitled to it, or presented no usable credential.
    /// As important as an acceptance: this is the line that shows somebody
    /// probing, or a pipeline still holding a credential that was narrowed.
    Refused,
    /// The caller was entitled to it and it failed - a 4xx that is the
    /// caller's own mistake, or a 5xx that is ours.
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Accepted => "accepted",
            Outcome::Refused => "refused",
            Outcome::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "accepted" => Some(Outcome::Accepted),
            "refused" => Some(Outcome::Refused),
            "failed" => Some(Outcome::Failed),
            _ => None,
        }
    }

    /// What an HTTP status means for the record, once it is known that the
    /// caller got past the authorisation check.
    pub fn from_status(status: u16) -> Self {
        match status {
            401 | 403 => Outcome::Refused,
            code if code < 400 => Outcome::Accepted,
            _ => Outcome::Failed,
        }
    }
}

/// One line of the log, as it is written and as it is read back.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuditEntry {
    /// Insertion order. The only ordering that is true under a clock with
    /// one-second resolution.
    pub id: i64,
    /// Unix seconds.
    pub at: i64,
    /// account, config_key or anonymous.
    pub actor_kind: String,
    /// The account's name. `None` only when the human API is unauthenticated.
    pub actor_account: Option<String>,
    /// The token's public id, or a configured key's fingerprint. Never any
    /// part of a credential.
    pub actor_credential: Option<String>,
    /// A stable name for the operation - `deploy`, `secret.write`,
    /// `token.mint` - taken from the route table so it does not drift with
    /// the URL.
    pub action: String,
    pub app_id: Option<String>,
    pub environment: Option<String>,
    /// The thing inside the application the call named: a secret's name, a
    /// route id, a token id, an account name. Never a value.
    pub target: Option<String>,
    pub method: String,
    pub path: String,
    /// Where the call came from. `None` when the server was built without
    /// connect info, which is why it is optional rather than invented.
    pub peer: Option<String>,
    pub outcome: Outcome,
    pub status: i64,
    /// Why it was refused, when it was. Empty for an ordinary acceptance.
    pub detail: Option<String>,
    /// The request id the answer echoed (`x-request-id`). `None` only for a
    /// line written before ids were recorded; every line written since
    /// carries the id the caller was given, a refusal's included. Left out
    /// of the wire form when absent, so a line written before the member
    /// existed still serialises byte for byte as it did.
    #[serde(rename = "requestId", skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub request_id: Option<String>,
}

/// A line about to be written. Separate from [`AuditEntry`] because the id
/// and the timestamp belong to the store, not to the caller.
#[derive(Debug, Clone)]
pub struct AuditRecord {
    pub actor_kind: String,
    pub actor_account: Option<String>,
    pub actor_credential: Option<String>,
    pub action: String,
    pub app_id: Option<String>,
    pub environment: Option<String>,
    pub target: Option<String>,
    pub method: String,
    pub path: String,
    pub peer: Option<String>,
    pub outcome: Outcome,
    pub status: u16,
    pub detail: Option<String>,
    pub request_id: Option<String>,
}

impl AuditRecord {
    /// Start a record from the caller the middleware resolved, so the three
    /// actor columns are filled from one place and cannot disagree.
    pub fn by(principal: &Principal) -> AuditRecordBuilder {
        AuditRecordBuilder {
            record: AuditRecord {
                actor_kind: principal.kind.as_str().to_string(),
                actor_account: principal.account.clone(),
                actor_credential: principal.credential_id.clone(),
                action: String::new(),
                app_id: None,
                environment: None,
                target: None,
                method: String::new(),
                path: String::new(),
                peer: None,
                outcome: Outcome::Accepted,
                status: 200,
                detail: None,
                request_id: None,
            },
        }
    }

    /// The record for a caller this control plane could not turn into an
    /// account, which is exactly the case an operator most wants to see.
    ///
    /// The credential is still worth naming when there was one: a revoked
    /// token names the token that was presented, and that is the line which
    /// says a withdrawn credential is still configured somewhere. Set it with
    /// [`AuditRecordBuilder::credential`].
    pub fn by_unknown_caller() -> AuditRecordBuilder {
        AuditRecordBuilder {
            record: AuditRecord {
                actor_kind: PrincipalKind::Anonymous.as_str().to_string(),
                actor_account: None,
                actor_credential: None,
                action: String::new(),
                app_id: None,
                environment: None,
                target: None,
                method: String::new(),
                path: String::new(),
                peer: None,
                outcome: Outcome::Refused,
                status: 401,
                detail: None,
                request_id: None,
            },
        }
    }
}

/// Assembles one record. Every field the model asks for has a setter here, so
/// a call site that forgets one is visible as a missing line rather than as a
/// silently empty column.
pub struct AuditRecordBuilder {
    record: AuditRecord,
}

impl AuditRecordBuilder {
    pub fn action(mut self, action: impl Into<String>) -> Self {
        self.record.action = action.into();
        self
    }

    pub fn request(mut self, method: &str, path: &str) -> Self {
        self.record.method = method.to_string();
        self.record.path = path.to_string();
        self
    }

    pub fn resource(mut self, app: Option<String>, environment: Option<String>) -> Self {
        self.record.app_id = app;
        self.record.environment = environment;
        self
    }

    /// Name the credential behind a call whose account could not be
    /// resolved. For a caller that did resolve, [`AuditRecord::by`] has
    /// already filled this in.
    pub fn credential(mut self, credential: Option<String>) -> Self {
        self.record.actor_credential = credential;
        self
    }

    pub fn target(mut self, target: Option<String>) -> Self {
        self.record.target = target;
        self
    }

    pub fn peer(mut self, peer: Option<String>) -> Self {
        self.record.peer = peer;
        self
    }

    pub fn outcome(mut self, outcome: Outcome, status: u16) -> Self {
        self.record.outcome = outcome;
        self.record.status = status;
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.record.detail = Some(detail.into());
        self
    }

    /// The request id the answer carried, already validated as a canonical
    /// id by whoever settled it: the log holds ids, never whatever text a
    /// caller happened to send in the header.
    pub fn request_id(mut self, request_id: Option<String>) -> Self {
        self.record.request_id = request_id;
        self
    }

    pub fn build(self) -> AuditRecord {
        self.record
    }
}

/// The filters a reader combines with "and". Paging is not one of them:
/// the position is an opaque cursor the list route hands out, bound to
/// exactly this set of filters.
///
/// `app_id` and `environment` are matched against what the audit line
/// recorded, which is the name in the request path at the time - a line
/// outlives the application it names, so there is no id to resolve it to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditQuery {
    pub app_id: Option<String>,
    pub environment: Option<String>,
    pub account: Option<String>,
    pub action: Option<String>,
    pub outcome: Option<Outcome>,
    /// Unix seconds, inclusive.
    pub since: Option<i64>,
    pub until: Option<i64>,
}

impl AuditQuery {
    /// The `WHERE` clauses and their bound values, in order. Shared by the
    /// count and the row read so the two can never disagree about which
    /// lines they are talking about.
    fn clauses(self) -> (Vec<String>, Vec<Box<dyn rusqlite::ToSql + Send>>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut binds: Vec<Box<dyn rusqlite::ToSql + Send>> = Vec::new();
        let mut push = |clause: &str, value: Box<dyn rusqlite::ToSql + Send>| {
            binds.push(value);
            clauses.push(format!("{clause} ?{}", binds.len()));
        };
        if let Some(app) = self.app_id {
            push("app_id =", Box::new(app));
        }
        if let Some(environment) = self.environment {
            push("environment =", Box::new(environment));
        }
        if let Some(account) = self.account {
            push("actor_account =", Box::new(account));
        }
        if let Some(action) = self.action {
            push("action =", Box::new(action));
        }
        if let Some(outcome) = self.outcome {
            push("outcome =", Box::new(outcome.as_str().to_string()));
        }
        if let Some(since) = self.since {
            push("at >=", Box::new(since));
        }
        if let Some(until) = self.until {
            push("at <=", Box::new(until));
        }
        (clauses, binds)
    }
}

/// Decode an audit cursor into `(at, id)`. The line id is an integer
/// column, so it travels as text in the position and is parsed back here;
/// anything else is a cursor that was never issued for this list.
fn audit_cursor(cursor: Option<PagePosition>) -> CatalogResult<(Option<i64>, i64)> {
    match cursor {
        Some(position) => {
            let id = position
                .identity
                .parse::<i64>()
                .map_err(|_| CatalogError::invalid("audit cursor position is malformed"))?;
            Ok((Some(position.sort_value), id))
        }
        None => Ok((None, 0)),
    }
}

pub struct AuditStore {
    db: Arc<Database>,
}

impl AuditStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Opaque list-cursor persistence for the audit list, backed by the
    /// same table the catalog lists use.
    pub fn page_cursors(&self) -> PageCursorStore {
        PageCursorStore::new(Arc::clone(&self.db))
    }

    /// Append one line.
    ///
    /// Failure to write is reported at `error` and swallowed: the request it
    /// describes has already happened, and turning an audit failure into a
    /// 500 would tell the caller their deploy failed when it did not. The log
    /// line is the compensating record, and it carries the whole event.
    ///
    /// Nothing prunes this table. That is deliberate: a retention rule is a
    /// way to lose evidence on a schedule, and the rows are small and written
    /// only for changes and refusals. An installation that needs a bound
    /// should copy the table out and truncate it at the file, which is the
    /// same level of access that could rewrite it anyway.
    pub async fn record(&self, record: AuditRecord) {
        let description = format!(
            "{} {} on {}/{} by {} -> {} ({})",
            record.action,
            record.method,
            record.app_id.as_deref().unwrap_or("-"),
            record.environment.as_deref().unwrap_or("-"),
            record.actor_account.as_deref().unwrap_or("anonymous"),
            record.outcome.as_str(),
            record.status
        );
        let at = now_secs();
        let written = self
            .db
            .with_connection(move |conn| {
                conn.execute(
                    "INSERT INTO audit_log \
                     (at, actor_kind, actor_account, actor_credential, action, app_id, \
                      environment, target, method, path, peer, outcome, status, detail, \
                      request_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, \
                             ?15)",
                    params![
                        at,
                        record.actor_kind,
                        record.actor_account,
                        record.actor_credential,
                        record.action,
                        record.app_id,
                        record.environment,
                        record.target,
                        record.method,
                        record.path,
                        record.peer,
                        record.outcome.as_str(),
                        record.status as i64,
                        record.detail,
                        record.request_id,
                    ],
                )?;
                Ok::<(), DatabaseError>(())
            })
            .await;

        if let Err(e) = written {
            error!(
                error = %e,
                event = %description,
                "Could not write an audit record; the call it describes still happened, and this log line is now the only trace of it"
            );
        }
    }

    /// One page of the log, newest first: `(at DESC, id DESC)`, the wall
    /// clock an operator reads and the insertion id to make equal seconds
    /// deterministic. The total is counted over the same filters in the
    /// same read transaction as the rows, so it is exact for the page.
    pub async fn query_page(
        &self,
        query: AuditQuery,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<AuditEntry>> {
        let fetch_limit = sqlite_fetch_limit(limit, "audit entries")?;
        let (before_at, before_id) = audit_cursor(cursor)?;
        self.db
            .with_connection(move |conn| {
                let transaction = conn.unchecked_transaction()?;
                let (mut clauses, mut binds) = query.clauses();
                let filter = if clauses.is_empty() {
                    String::new()
                } else {
                    format!(" WHERE {}", clauses.join(" AND "))
                };
                let total = transaction.query_row(
                    &format!("SELECT COUNT(*) FROM audit_log{filter}"),
                    rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())),
                    |row| row.get::<_, i64>(0),
                )?;

                binds.push(Box::new(before_at));
                let at_index = binds.len();
                binds.push(Box::new(before_id));
                let id_index = binds.len();
                clauses.push(format!(
                    "(?{at_index} IS NULL OR at < ?{at_index} \
                       OR (at = ?{at_index} AND id < ?{id_index}))"
                ));
                binds.push(Box::new(fetch_limit));
                let limit_index = binds.len();
                let sql = format!(
                    "SELECT {AUDIT_COLUMNS} FROM audit_log WHERE {} \
                     ORDER BY at DESC, id DESC LIMIT ?{limit_index}",
                    clauses.join(" AND ")
                );
                let mut stmt = transaction.prepare(&sql)?;
                let rows = stmt
                    .query_map(
                        rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())),
                        read_entry,
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(stmt);
                transaction.commit()?;
                finish_page(rows, limit, total, "audit entries")
            })
            .await
    }

    /// How many lines the log holds, said once at startup so an operator
    /// knows whether they are looking at a fresh table or at a history.
    pub async fn announce(&self) {
        match self
            .db
            .with_connection(|conn| {
                Ok::<i64, DatabaseError>(conn.query_row(
                    "SELECT COUNT(*) FROM audit_log",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
        {
            Ok(lines) => info!(
                lines,
                "Audit log ready; every deploy, rollback, secret change, token mint, account change and route change is recorded here, refusals included, and no endpoint of this control plane can delete a line"
            ),
            Err(e) => warn!(
                error = %e,
                "Could not count the audit log; recording will still be attempted, but a control plane that cannot read this table may not be able to write it either"
            ),
        }
    }
}

fn read_entry(row: &Row<'_>) -> rusqlite::Result<AuditEntry> {
    let outcome: String = row.get(12)?;
    Ok(AuditEntry {
        id: row.get(0)?,
        at: row.get(1)?,
        actor_kind: row.get(2)?,
        actor_account: row.get(3)?,
        actor_credential: row.get(4)?,
        action: row.get(5)?,
        app_id: row.get(6)?,
        environment: row.get(7)?,
        target: row.get(8)?,
        method: row.get(9)?,
        path: row.get(10)?,
        peer: row.get(11)?,
        outcome: Outcome::parse(&outcome).unwrap_or(Outcome::Failed),
        status: row.get(13)?,
        detail: row.get(14)?,
        request_id: row.get(15)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Authority, Role};

    async fn store() -> AuditStore {
        let db = Database::init(":memory:").await.unwrap();
        AuditStore::new(Arc::new(db))
    }

    async fn page(
        store: &AuditStore,
        query: AuditQuery,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogPage<AuditEntry> {
        store.query_page(query, limit, cursor).await.unwrap()
    }

    fn alice() -> Principal {
        Principal {
            kind: PrincipalKind::Account,
            account: Some("alice".to_string()),
            role: Some(Role::Owner),
            credential_id: Some("0011223344556677".to_string()),
            authority: Authority::Unscoped,
        }
    }

    /// The definition of a line: who, what, which application and
    /// environment, from where, and how it ended - all of it readable back.
    #[tokio::test]
    async fn a_line_carries_who_what_where_and_how_it_ended() {
        let store = store().await;
        store
            .record(
                AuditRecord::by(&alice())
                    .action("deploy")
                    .request("POST", "/api/apps/demo/environments/production/deployments")
                    .resource(Some("demo".to_string()), Some("production".to_string()))
                    .peer(Some("10.0.0.4:51234".to_string()))
                    .outcome(Outcome::Accepted, 200)
                    .request_id(Some("018f2518-7e38-7f80-8000-000000000001".to_string()))
                    .build(),
            )
            .await;

        let lines = page(&store, AuditQuery::default(), 100, None).await.items;
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert_eq!(line.actor_account.as_deref(), Some("alice"));
        assert_eq!(line.actor_credential.as_deref(), Some("0011223344556677"));
        assert_eq!(line.action, "deploy");
        assert_eq!(line.app_id.as_deref(), Some("demo"));
        assert_eq!(line.environment.as_deref(), Some("production"));
        assert_eq!(line.peer.as_deref(), Some("10.0.0.4:51234"));
        assert_eq!(line.outcome, Outcome::Accepted);
        assert_eq!(line.status, 200);
        assert_eq!(
            line.request_id.as_deref(),
            Some("018f2518-7e38-7f80-8000-000000000001")
        );
        // On the wire the id is `requestId`, like the one the error
        // envelope carries, so a client joins the two by one name.
        let dump = serde_json::to_value(line).unwrap();
        assert_eq!(dump["requestId"], "018f2518-7e38-7f80-8000-000000000001");
    }

    /// A log written before request ids were recorded is read through the
    /// same store: the column arrives on the next start, the old lines read
    /// back with no id, and new lines carry theirs.
    #[tokio::test]
    async fn an_older_log_reads_back_with_no_request_id_and_keeps_taking_lines() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("control.sqlite");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE audit_log (
                    id               INTEGER PRIMARY KEY AUTOINCREMENT,
                    at               INTEGER NOT NULL,
                    actor_kind       TEXT NOT NULL,
                    actor_account    TEXT,
                    actor_credential TEXT,
                    action           TEXT NOT NULL,
                    app_id           TEXT,
                    environment      TEXT,
                    target           TEXT,
                    method           TEXT NOT NULL,
                    path             TEXT NOT NULL,
                    peer             TEXT,
                    outcome          TEXT NOT NULL,
                    status           INTEGER NOT NULL,
                    detail           TEXT
                );
                INSERT INTO audit_log
                    (at, actor_kind, actor_account, action, app_id, environment, method,
                     path, outcome, status)
                    VALUES (1700000000, 'account', 'alice', 'deploy', 'demo', 'production',
                            'POST', '/api/apps/demo/environments/production/deployments',
                            'accepted', 201);",
            )
            .unwrap();
        }

        let store = AuditStore::new(Arc::new(Database::init(&path).await.unwrap()));
        let lines = page(&store, AuditQuery::default(), 100, None).await.items;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].action, "deploy");
        assert_eq!(lines[0].request_id, None);
        // A line from before ids were recorded keeps its old wire form: the
        // member is left out rather than sent as null.
        assert!(
            serde_json::to_value(&lines[0])
                .unwrap()
                .get("requestId")
                .is_none()
        );

        store
            .record(
                AuditRecord::by(&alice())
                    .action("rollback")
                    .request("POST", "/api/v1/apps/demo/environments/production/rollback")
                    .outcome(Outcome::Accepted, 201)
                    .request_id(Some("018f2518-7e38-7f80-8000-000000000002".to_string()))
                    .build(),
            )
            .await;
        let lines = page(&store, AuditQuery::default(), 100, None).await.items;
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0].request_id.as_deref(),
            Some("018f2518-7e38-7f80-8000-000000000002")
        );
        assert_eq!(lines[1].request_id, None);
    }

    /// A refused call is as important as an accepted one, and has to be
    /// findable on its own.
    #[tokio::test]
    async fn a_refusal_is_recorded_and_can_be_asked_for_by_itself() {
        let store = store().await;
        store
            .record(
                AuditRecord::by_unknown_caller()
                    .action("secret.write")
                    .request(
                        "PUT",
                        "/api/apps/demo/environments/production/secrets/TOKEN",
                    )
                    .resource(Some("demo".to_string()), Some("production".to_string()))
                    .target(Some("TOKEN".to_string()))
                    .peer(Some("203.0.113.9:4444".to_string()))
                    .outcome(Outcome::Refused, 401)
                    .detail("no credential was presented")
                    .build(),
            )
            .await;
        store
            .record(
                AuditRecord::by(&alice())
                    .action("deploy")
                    .request("POST", "/api/apps/demo/environments/production/deployments")
                    .resource(Some("demo".to_string()), Some("production".to_string()))
                    .outcome(Outcome::Accepted, 200)
                    .build(),
            )
            .await;

        let refusals = page(
            &store,
            AuditQuery {
                outcome: Some(Outcome::Refused),
                ..Default::default()
            },
            100,
            None,
        )
        .await
        .items;
        assert_eq!(refusals.len(), 1);
        assert_eq!(refusals[0].target.as_deref(), Some("TOKEN"));
        assert_eq!(refusals[0].actor_kind, "anonymous");
        assert_eq!(
            refusals[0].detail.as_deref(),
            Some("no credential was presented")
        );
    }

    /// A secret write names the secret and never its value. The recorder is
    /// given the name from the path and is never handed a body, so there is
    /// no route by which a value could arrive here - this is the assertion
    /// that keeps it that way.
    #[tokio::test]
    async fn a_secret_write_names_the_secret_and_not_its_value() {
        let store = store().await;
        store
            .record(
                AuditRecord::by(&alice())
                    .action("secret.write")
                    .request(
                        "PUT",
                        "/api/apps/demo/environments/production/secrets/DB_URL",
                    )
                    .resource(Some("demo".to_string()), Some("production".to_string()))
                    .target(Some("DB_URL".to_string()))
                    .outcome(Outcome::Accepted, 200)
                    .build(),
            )
            .await;
        let dump =
            serde_json::to_string(&page(&store, AuditQuery::default(), 100, None).await.items)
                .unwrap();
        assert!(dump.contains("DB_URL"));
        assert!(
            !dump.contains("postgres://"),
            "no part of a secret value may reach the audit log"
        );
    }

    /// The filters an operator actually uses after an incident: this
    /// application, this person, this operation, this window.
    #[tokio::test]
    async fn the_log_is_queryable_by_application_account_action_and_time() {
        let store = store().await;
        for (app, action, account) in [
            ("demo", "deploy", "alice"),
            ("demo", "rollback", "alice"),
            ("other", "deploy", "bob"),
        ] {
            let principal = Principal {
                account: Some(account.to_string()),
                ..alice()
            };
            store
                .record(
                    AuditRecord::by(&principal)
                        .action(action)
                        .request("POST", "/api/apps")
                        .resource(Some(app.to_string()), Some("production".to_string()))
                        .outcome(Outcome::Accepted, 200)
                        .build(),
                )
                .await;
        }

        let by_app = store
            .query_page(
                AuditQuery {
                    app_id: Some("demo".to_string()),
                    ..Default::default()
                },
                100,
                None,
            )
            .await
            .unwrap()
            .items;
        assert_eq!(by_app.len(), 2);
        // Newest first, so an operator reads what just happened at the top.
        assert_eq!(by_app[0].action, "rollback");

        let by_account = store
            .query_page(
                AuditQuery {
                    account: Some("bob".to_string()),
                    ..Default::default()
                },
                100,
                None,
            )
            .await
            .unwrap()
            .items;
        assert_eq!(by_account.len(), 1);
        assert_eq!(by_account[0].app_id.as_deref(), Some("other"));

        let by_action = store
            .query_page(
                AuditQuery {
                    action: Some("deploy".to_string()),
                    ..Default::default()
                },
                100,
                None,
            )
            .await
            .unwrap()
            .items;
        assert_eq!(by_action.len(), 2);

        let future = store
            .query_page(
                AuditQuery {
                    since: Some(now_secs() + 3600),
                    ..Default::default()
                },
                100,
                None,
            )
            .await
            .unwrap()
            .items;
        assert!(future.is_empty());
    }

    /// A page is bounded, its total counts the whole filtered log, and the
    /// cursor position it ends on resumes exactly one line later.
    #[tokio::test]
    async fn a_query_is_bounded_and_pages_through_a_keyset() {
        let store = store().await;
        for index in 0..5 {
            store
                .record(
                    AuditRecord::by(&alice())
                        .action(format!("action.{index}"))
                        .request("POST", "/api/apps")
                        .outcome(Outcome::Accepted, 200)
                        .build(),
                )
                .await;
        }

        let first = page(&store, AuditQuery::default(), 2, None).await;
        assert_eq!(first.items.len(), 2);
        assert_eq!(first.total, 5);
        assert!(first.has_more);
        assert_eq!(first.items[0].action, "action.4");

        let last = &first.items[1];
        let next = page(
            &store,
            AuditQuery::default(),
            2,
            Some(PagePosition {
                sort_value: last.at,
                identity: last.id.to_string(),
                secondary: None,
            }),
        )
        .await;
        assert_eq!(next.items[0].action, "action.2");
        assert_eq!(next.total, 5);

        let end = page(
            &store,
            AuditQuery::default(),
            2,
            Some(PagePosition {
                sort_value: next.items[1].at,
                identity: next.items[1].id.to_string(),
                secondary: None,
            }),
        )
        .await;
        assert_eq!(end.items.len(), 1);
        assert!(!end.has_more);

        assert!(
            store
                .query_page(AuditQuery::default(), 0, None)
                .await
                .is_err()
        );
        assert!(
            store
                .query_page(AuditQuery::default(), 1_001, None)
                .await
                .is_err()
        );
        assert!(
            store
                .query_page(
                    AuditQuery::default(),
                    2,
                    Some(PagePosition {
                        sort_value: 0,
                        identity: "not-a-line-id".to_string(),
                        secondary: None,
                    }),
                )
                .await
                .is_err()
        );
    }
}
