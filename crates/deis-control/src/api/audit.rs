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

use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::api::error::{ApiError, ApiResult};
use crate::audit::{AuditEntry, AuditQuery, AuditStore, MAX_QUERY_LIMIT, Outcome};

#[derive(Clone)]
pub struct AuditApiState {
    pub audit: Arc<AuditStore>,
}

/// The filters an operator reaches for after an incident. Every one of them
/// is optional, and the answer is newest first.
#[derive(Debug, Deserialize)]
pub struct AuditQueryParams {
    #[serde(default)]
    pub app: Option<String>,
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
    /// Unix seconds.
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub until: Option<i64>,
    /// Page backwards: only lines older than this one.
    #[serde(default)]
    pub before: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct AuditResponse {
    pub entries: Vec<AuditEntry>,
    pub total: usize,
    /// The largest page this control plane will serve, so a caller paging
    /// backwards does not have to discover it by being truncated.
    pub max_limit: usize,
}

/// GET /api/audit
pub async fn read_audit(
    State(state): State<AuditApiState>,
    Query(params): Query<AuditQueryParams>,
) -> ApiResult<Json<AuditResponse>> {
    let outcome = match params.outcome.as_deref() {
        None => None,
        Some(text) => Some(Outcome::parse(text).ok_or_else(|| {
            ApiError::BadRequest(format!(
                "{text:?} is not an outcome; a call is recorded as accepted, refused or failed"
            ))
        })?),
    };

    let entries = state
        .audit
        .query(AuditQuery {
            app_id: params.app,
            environment: params.environment,
            account: params.account,
            action: params.action,
            outcome,
            since: params.since,
            until: params.until,
            before: params.before,
            limit: params.limit,
        })
        .await?;

    Ok(Json(AuditResponse {
        total: entries.len(),
        entries,
        max_limit: MAX_QUERY_LIMIT,
    }))
}
