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

use crate::catalog::model::now_secs;
use crate::database::{Database, DatabaseError};
use crate::identity::{Principal, PrincipalKind};

/// Most lines one query will return, whatever the caller asks for. A reader
/// that wants more pages back through `before`.
pub const MAX_QUERY_LIMIT: usize = 500;

const DEFAULT_QUERY_LIMIT: usize = 50;

const AUDIT_COLUMNS: &str = "id, at, actor_kind, actor_account, actor_credential, action, \
                             app_id, environment, target, method, path, peer, outcome, \
                             status, detail";

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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

    pub fn build(self) -> AuditRecord {
        self.record
    }
}

/// What a reader is asking the log for.
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    pub app_id: Option<String>,
    pub environment: Option<String>,
    pub account: Option<String>,
    pub action: Option<String>,
    pub outcome: Option<Outcome>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// Return only lines older than this id, which is how a reader pages
    /// backwards through a log that is still being written to.
    pub before: Option<i64>,
    pub limit: Option<usize>,
}

pub struct AuditStore {
    db: Arc<Database>,
}

impl AuditStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
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
                      environment, target, method, path, peer, outcome, status, detail) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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

    /// Read the log back, newest first.
    pub async fn query(&self, query: AuditQuery) -> Result<Vec<AuditEntry>, DatabaseError> {
        let limit = query
            .limit
            .unwrap_or(DEFAULT_QUERY_LIMIT)
            .clamp(1, MAX_QUERY_LIMIT);

        self.db
            .with_connection(move |conn| {
                let mut sql = format!("SELECT {AUDIT_COLUMNS} FROM audit_log");
                let mut clauses: Vec<String> = Vec::new();
                let mut binds: Vec<Box<dyn rusqlite::ToSql + Send>> = Vec::new();

                let mut push = |clause: &str, value: Box<dyn rusqlite::ToSql + Send>| {
                    binds.push(value);
                    clauses.push(format!("{clause} ?{}", binds.len()));
                };
                if let Some(app) = query.app_id {
                    push("app_id =", Box::new(app));
                }
                if let Some(environment) = query.environment {
                    push("environment =", Box::new(environment));
                }
                if let Some(account) = query.account {
                    push("actor_account =", Box::new(account));
                }
                if let Some(action) = query.action {
                    push("action =", Box::new(action));
                }
                if let Some(outcome) = query.outcome {
                    push("outcome =", Box::new(outcome.as_str().to_string()));
                }
                if let Some(since) = query.since {
                    push("at >=", Box::new(since));
                }
                if let Some(until) = query.until {
                    push("at <=", Box::new(until));
                }
                if let Some(before) = query.before {
                    push("id <", Box::new(before));
                }
                if !clauses.is_empty() {
                    sql.push_str(" WHERE ");
                    sql.push_str(&clauses.join(" AND "));
                }
                sql.push_str(&format!(" ORDER BY id DESC LIMIT {limit}"));

                let mut stmt = conn.prepare(&sql)?;
                let params = rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref()));
                let rows = stmt
                    .query_map(params, read_entry)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, DatabaseError>(rows)
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
                    .build(),
            )
            .await;

        let lines = store.query(AuditQuery::default()).await.unwrap();
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

        let refusals = store
            .query(AuditQuery {
                outcome: Some(Outcome::Refused),
                ..Default::default()
            })
            .await
            .unwrap();
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
            serde_json::to_string(&store.query(AuditQuery::default()).await.unwrap()).unwrap();
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
            .query(AuditQuery {
                app_id: Some("demo".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(by_app.len(), 2);
        // Newest first, so an operator reads what just happened at the top.
        assert_eq!(by_app[0].action, "rollback");

        let by_account = store
            .query(AuditQuery {
                account: Some("bob".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(by_account.len(), 1);
        assert_eq!(by_account[0].app_id.as_deref(), Some("other"));

        let by_action = store
            .query(AuditQuery {
                action: Some("deploy".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(by_action.len(), 2);

        let future = store
            .query(AuditQuery {
                since: Some(now_secs() + 3600),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(future.is_empty());
    }

    /// A reader must not be able to ask for the whole table in one call, and
    /// must be able to walk backwards through it.
    #[tokio::test]
    async fn a_query_is_bounded_and_pages_backwards() {
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

        let first = store
            .query(AuditQuery {
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].action, "action.4");

        let next = store
            .query(AuditQuery {
                limit: Some(2),
                before: Some(first[1].id),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(next[0].action, "action.2");

        let capped = store
            .query(AuditQuery {
                limit: Some(usize::MAX),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(capped.len() <= MAX_QUERY_LIMIT);
    }
}
