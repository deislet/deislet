//! Reading the audit log back.
//!
//! One route, `GET /api/audit`, and deliberately no others: there is no
//! endpoint that writes a line by hand, none that edits one and none that
//! deletes one. The only writer is the authorisation gate
//! ([`crate::api::authz::authorize`]), which records what actually happened
//! rather than what a caller says happened.
//!
//! It answers to `admin:*/*`, because one line names an application, an
//! environment and a person, and a log filtered per caller would be a log an
//! operator could not use to find the caller they are looking for.
//!
//! Each item is one [`AuditEntry`]: who (`actor_kind`, `actor_account`,
//! `actor_credential`), what (`action`, `method`, `path`, `app_id`,
//! `environment`, `target`), from where (`peer`), how it ended (`outcome`,
//! `status`, `detail`), and `requestId` - the `x-request-id` the answer
//! echoed, null for lines written before it was recorded or for a refusal
//! the gate answered without one. It is the same id the v1 error envelope
//! carries as `requestId`, so a failed call is found in the log by the value
//! the caller already holds.

use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;
use std::sync::Arc;
use utoipa::IntoParams;

use super::openapi;
use crate::api::error::{ApiError, ApiResult};
use crate::api::pagination::{ControlListPage, ListQuery, list_page};
use crate::audit::{AuditEntry, AuditQuery, AuditStore, Outcome};
use crate::catalog::PagePosition;

#[derive(Clone)]
pub struct AuditApiState {
    pub audit: Arc<AuditStore>,
}

/// What `GET /api/audit` accepts: the page controls of every list route
/// plus the filters an operator reaches for after an incident. Every filter
/// is optional, they combine with "and", and the answer is newest first.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct AuditListQuery {
    /// Page size, `1..=1000`; absent means 100.
    #[param(minimum = 1, maximum = 1000)]
    pub limit: Option<usize>,
    /// The `page.nextCursor` of the previous answer. Opaque, bound to the
    /// collection and the filters it was issued for, and refused with 400
    /// `cursor-invalid` anywhere else.
    pub cursor: Option<String>,
    /// The application's name as the audit line recorded it from the
    /// request path. A line outlives the application it names, so it is
    /// matched verbatim rather than resolved to an id.
    #[serde(default)]
    pub app: Option<String>,
    /// The environment's name as the audit line recorded it, matched
    /// verbatim like `app`.
    #[serde(default)]
    pub environment: Option<String>,
    /// The account's name, as it appears in the `actor_account` column.
    #[serde(default)]
    pub account: Option<String>,
    /// One of the action names the route table records - `deploy`,
    /// `secret.write`, `token.mint`.
    #[serde(default)]
    pub action: Option<String>,
    /// `accepted`, `refused` or `failed`.
    #[serde(default)]
    pub outcome: Option<String>,
    /// Unix seconds, inclusive.
    #[serde(default)]
    pub since: Option<i64>,
    /// Unix seconds, inclusive; the upper bound of the window `since` opens.
    #[serde(default)]
    pub until: Option<i64>,
}

impl AuditListQuery {
    fn page_query(&self) -> ListQuery {
        ListQuery {
            limit: self.limit,
            cursor: self.cursor.clone(),
        }
    }

    fn filter(&self) -> ApiResult<AuditQuery> {
        let outcome = match self.outcome.as_deref() {
            None => None,
            Some(text) => Some(Outcome::parse(text).ok_or_else(|| {
                ApiError::BadRequest(format!(
                    "{text:?} is not an outcome; a call is recorded as accepted, refused or failed"
                ))
            })?),
        };
        Ok(AuditQuery {
            app_id: self.app.clone(),
            environment: self.environment.clone(),
            account: self.account.clone(),
            action: self.action.clone(),
            outcome,
            since: self.since,
            until: self.until,
        })
    }
}

/// The cursor binding of one audit listing: the whole installation, every
/// filter's exact value, and the sort. Text values are written as JSON
/// string literals so a value containing the separators cannot read as a
/// different set of filters; an absent filter is `all` (or `none` for the
/// time bounds).
fn cursor_binding(filter: &AuditQuery) -> String {
    fn text(value: Option<&str>) -> String {
        match value {
            Some(value) => serde_json::Value::String(value.to_string()).to_string(),
            None => "all".to_string(),
        }
    }
    fn number(value: Option<i64>) -> String {
        value.map_or_else(|| "none".to_string(), |value| value.to_string())
    }
    format!(
        "scope=global;filter=app:{},environment:{},account:{},action:{},outcome:{},since:{},until:{};sort=at-desc,id-desc-v1",
        text(filter.app_id.as_deref()),
        text(filter.environment.as_deref()),
        text(filter.account.as_deref()),
        text(filter.action.as_deref()),
        filter.outcome.map_or("all", Outcome::as_str),
        number(filter.since),
        number(filter.until),
    )
}

/// GET /api/audit
#[utoipa::path(
    get,
    path = "/api/v1/audit",
    operation_id = "listAuditLog",
    tag = "product",
    params(AuditListQuery),
    responses(
        (status = 200, description = "One page of audit lines, newest first.", body = ControlListPage<AuditEntry>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn read_audit(
    State(state): State<AuditApiState>,
    Query(query): Query<AuditListQuery>,
) -> ApiResult<Json<ControlListPage<AuditEntry>>> {
    let filter = query.filter()?;
    let collection = "audit-entries";
    let binding = cursor_binding(&filter);
    let page_query = query.page_query();
    let limit = page_query.bounded_limit()?;
    let cursor = page_query
        .position(&state.audit.page_cursors(), collection, &binding)
        .await?;
    let page = state.audit.query_page(filter, limit, cursor).await?;
    let response = list_page(
        &state.audit.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |entry| PagePosition {
            sort_value: entry.at,
            identity: entry.id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::catalog::tests::json;
    use crate::audit::AuditRecord;
    use crate::database::Database;
    use crate::identity::{Authority, Principal, PrincipalKind, Role};
    use axum::{Router, http::StatusCode, routing::get};
    use serde_json::Value;

    async fn harness() -> (Router, Arc<AuditStore>) {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let audit = Arc::new(AuditStore::new(database));
        let router = Router::new()
            .route("/api/audit", get(read_audit))
            .with_state(AuditApiState {
                audit: Arc::clone(&audit),
            });
        (router, audit)
    }

    fn actor(account: &str) -> Principal {
        Principal {
            kind: PrincipalKind::Account,
            account: Some(account.to_string()),
            role: Some(Role::Owner),
            credential_id: Some("0011223344556677".to_string()),
            authority: Authority::Unscoped,
        }
    }

    fn actions(page: &Value) -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["action"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_binding_names_every_filter_unambiguously() {
        assert_eq!(
            cursor_binding(&AuditQuery::default()),
            "scope=global;filter=app:all,environment:all,account:all,action:all,outcome:all,since:none,until:none;sort=at-desc,id-desc-v1"
        );
        // A value that spells another filter stays one quoted value.
        let tricky = cursor_binding(&AuditQuery {
            app_id: Some("demo,environment:production".to_string()),
            outcome: Some(Outcome::Refused),
            since: Some(7),
            ..Default::default()
        });
        assert_ne!(
            tricky,
            cursor_binding(&AuditQuery {
                app_id: Some("demo".to_string()),
                environment: Some("production".to_string()),
                outcome: Some(Outcome::Refused),
                since: Some(7),
                ..Default::default()
            })
        );
        assert!(tricky.contains(r#"app:"demo,environment:production",environment:all"#));
        assert!(tricky.contains("outcome:refused,since:7,until:none"));
    }

    #[tokio::test]
    async fn the_audit_log_pages_newest_first_through_the_unified_envelope() {
        let (router, audit) = harness().await;
        for (index, (app, outcome, status)) in [
            ("demo", Outcome::Accepted, 200),
            ("other", Outcome::Refused, 403),
            ("demo", Outcome::Accepted, 200),
            ("demo", Outcome::Refused, 403),
            ("other", Outcome::Accepted, 201),
        ]
        .into_iter()
        .enumerate()
        {
            audit
                .record(
                    AuditRecord::by(&actor("alice"))
                        .action(format!("action.{index}"))
                        .request("POST", "/api/apps")
                        .resource(Some(app.to_string()), Some("production".to_string()))
                        .outcome(outcome, status)
                        .request_id(
                            (index == 3)
                                .then(|| "018f2518-7e38-7f80-8000-000000000003".to_string()),
                        )
                        .build(),
                )
                .await;
        }

        let (status, first) = json(&router, "GET", "/api/audit?limit=2", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["schemaVersion"], 1);
        assert_eq!(first["page"]["limit"], 2);
        assert_eq!(first["page"]["count"], 2);
        assert_eq!(first["page"]["total"], 5);
        assert_eq!(actions(&first), ["action.4", "action.3"]);
        let cursor = first["page"]["nextCursor"].as_str().unwrap().to_string();
        assert_eq!(cursor.len(), 43);

        // Change the page size mid-walk; the position is what the cursor
        // carries, not the size.
        let (status, second) = json(
            &router,
            "GET",
            &format!("/api/audit?limit=3&cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(actions(&second), ["action.2", "action.1", "action.0"]);
        assert_eq!(second["page"]["total"], 5);
        assert!(second["page"]["nextCursor"].is_null());

        // The cursor is bound to its filters: any other filter set refuses it.
        for uri in [
            format!("/api/audit?app=demo&cursor={cursor}"),
            format!("/api/audit?outcome=refused&cursor={cursor}"),
            format!("/api/audit?since=1&cursor={cursor}"),
        ] {
            let (status, _) = json(&router, "GET", &uri, Value::Null).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }

        // A filtered listing counts only its own lines, and its own cursor
        // resumes inside the same filter.
        let (status, refused) = json(
            &router,
            "GET",
            "/api/audit?app=demo&outcome=refused",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{refused}");
        assert_eq!(refused["page"]["total"], 1);
        assert_eq!(actions(&refused), ["action.3"]);
        assert_eq!(refused["items"][0]["status"], 403);
        assert_eq!(
            refused["items"][0]["requestId"],
            "018f2518-7e38-7f80-8000-000000000003"
        );
        // A line recorded before ids existed keeps its old wire form: the
        // member is left out, never sent as null, so the legacy list is
        // byte for byte what it was for those rows.
        assert!(first["items"][0].get("requestId").is_none());

        let (status, demo) = json(&router, "GET", "/api/audit?app=demo&limit=1", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{demo}");
        assert_eq!(demo["page"]["total"], 3);
        assert_eq!(actions(&demo), ["action.3"]);
        let demo_cursor = demo["page"]["nextCursor"].as_str().unwrap().to_string();
        let (status, rest) = json(
            &router,
            "GET",
            &format!("/api/audit?app=demo&limit=10&cursor={demo_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rest}");
        assert_eq!(actions(&rest), ["action.2", "action.0"]);
        assert!(rest["page"]["nextCursor"].is_null());
        let (status, _) = json(
            &router,
            "GET",
            &format!("/api/audit?app=other&cursor={demo_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // What the route no longer understands, and what it never did.
        let (status, _) = json(&router, "GET", "/api/audit?before=3", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(&router, "GET", "/api/audit?limit=501", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "the unified bound is 1000");
        let (status, _) = json(&router, "GET", "/api/audit?limit=1001", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(&router, "GET", "/api/audit?outcome=maybe", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
