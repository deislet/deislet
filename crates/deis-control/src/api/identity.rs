//! Accounts and tokens over HTTP: what `deis account` and `deis token` call.
//!
//! Everything here answers to `admin:*/*` - see [`super::authz::classify`] -
//! because these routes are how somebody hands out authority, and an account
//! that could mint its own tokens would have no ceiling at all.
//!
//! The one rule this module exists to keep: a token's secret appears in
//! exactly one response, the one that created it. There is no endpoint that
//! reads a token back, because there is nothing to read - the database holds
//! a digest. That is enforced by the types: [`ApiToken`] has no field a
//! secret could live in, and only [`create_token`] returns [`MintResponse`].

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::api::error::{ApiError, ApiResult};
use crate::identity::{ApiToken, Authority, IdentityStore, Role, Scope, store::Account};

#[derive(Clone)]
pub struct IdentityApiState {
    pub identity: Arc<IdentityStore>,
}

impl From<crate::identity::IdentityError> for ApiError {
    fn from(err: crate::identity::IdentityError) -> Self {
        use crate::identity::IdentityError;
        let message = err.to_string();
        match err {
            IdentityError::NotFound(_) => ApiError::NotFound(message),
            IdentityError::Conflict(_) => ApiError::Conflict(message),
            IdentityError::Invalid(_) => ApiError::BadRequest(message),
            IdentityError::Database(_) | IdentityError::Backend(_) => {
                ApiError::DatabaseError(message)
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AccountsResponse {
    pub accounts: Vec<Account>,
    pub total: usize,
}

#[derive(Debug, Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
    /// One of the names in [`Role`]. Required rather than defaulted: the
    /// difference between `viewer` and `owner` is the whole point, and a
    /// default would be the one somebody forgot to think about.
    pub role: String,
}

/// GET /api/accounts
pub async fn list_accounts(
    State(state): State<IdentityApiState>,
) -> ApiResult<Json<AccountsResponse>> {
    let accounts = state.identity.list_accounts().await?;
    Ok(Json(AccountsResponse {
        total: accounts.len(),
        accounts,
    }))
}

/// POST /api/accounts
pub async fn create_account(
    State(state): State<IdentityApiState>,
    Json(request): Json<CreateAccountRequest>,
) -> ApiResult<Json<Account>> {
    let role = parse_role(&request.role)?;
    let account = state.identity.create_account(&request.name, role).await?;
    Ok(Json(account))
}

/// GET /api/accounts/:name
pub async fn get_account(
    State(state): State<IdentityApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Account>> {
    Ok(Json(state.identity.get_account(&name).await?))
}

#[derive(Debug, Deserialize)]
pub struct UpdateAccountRequest {
    /// Absent means "leave it".
    #[serde(default)]
    pub role: Option<String>,
    /// Absent means "leave it". `true` refuses every token of this account
    /// without revoking any of them.
    #[serde(default)]
    pub disabled: Option<bool>,
}

/// PATCH /api/accounts/:name
pub async fn update_account(
    State(state): State<IdentityApiState>,
    Path(name): Path<String>,
    Json(request): Json<UpdateAccountRequest>,
) -> ApiResult<Json<Account>> {
    let role = request.role.as_deref().map(parse_role).transpose()?;
    let account = state
        .identity
        .update_account(&name, role, request.disabled)
        .await?;
    Ok(Json(account))
}

#[derive(Debug, Deserialize)]
pub struct CreateTokenRequest {
    /// What this credential is for, in a word: `ci-preview`, `alice-laptop`.
    /// It is printed in every listing, so it is how a person decides which
    /// one to revoke.
    pub name: String,
    /// The scope list. Absent means the account's role over every
    /// application (see [`Role::default_scopes`]); an explicit empty list
    /// means a token that can do nothing yet.
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// Ask for a credential subject to no scope at all - the old flat
    /// `api_keys` behaviour. Spelled out rather than implied by an empty
    /// scope list, because "everything" and "nothing" must not be one typo
    /// apart.
    #[serde(default)]
    pub unscoped: bool,
}

/// What creating a token answers. The only place a token's secret is ever
/// serialised.
#[derive(Debug, Serialize)]
pub struct MintResponse {
    pub token: ApiToken,
    /// The credential itself, in full. It is not stored anywhere in this form
    /// and cannot be shown again; a caller that loses it revokes this token
    /// and mints another.
    pub secret: String,
    /// Said in the response as well as in the log, because the person reading
    /// it is at a terminal and about to paste it somewhere.
    pub shown_once: &'static str,
}

/// POST /api/accounts/:name/tokens
pub async fn create_token(
    State(state): State<IdentityApiState>,
    Path(account): Path<String>,
    Json(request): Json<CreateTokenRequest>,
) -> ApiResult<Json<MintResponse>> {
    let authority = match (request.unscoped, request.scopes) {
        (true, Some(scopes)) if !scopes.is_empty() => {
            return Err(ApiError::BadRequest(
                "a token is either unscoped or carries scopes, not both: an unscoped credential is \
                 the old flat api_keys behaviour and a scope list would say nothing about it"
                    .to_string(),
            ));
        }
        (true, _) => Authority::Unscoped,
        (false, None) => {
            let account = state.identity.get_account(&account).await?;
            Authority::Scoped(account.role.default_scopes())
        }
        (false, Some(scopes)) => Authority::Scoped(
            scopes
                .iter()
                .map(|text| Scope::parse(text))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| ApiError::BadRequest(e.to_string()))?,
        ),
    };

    let (token, secret) = state
        .identity
        .create_token(&account, &request.name, authority)
        .await?;
    Ok(Json(MintResponse {
        token,
        secret,
        shown_once: "This is the only time this control plane will show this credential. It is \
                     stored as a SHA-256 and cannot be recovered; if it is lost, revoke this token \
                     and mint another.",
    }))
}

#[derive(Debug, Deserialize)]
pub struct ListTokensQuery {
    #[serde(default)]
    pub account: Option<String>,
    /// Include credentials that no longer work. Off by default so a listing
    /// answers "what can reach this platform right now".
    #[serde(default)]
    pub include_revoked: bool,
}

#[derive(Debug, Serialize)]
pub struct TokensResponse {
    pub tokens: Vec<ApiToken>,
    pub total: usize,
    /// How many of the listed credentials answer to no scope at all. Served
    /// as a number so a dashboard, a script or `deis token list` can put it
    /// in front of an operator without counting for itself.
    pub unscoped: usize,
}

/// GET /api/tokens
pub async fn list_tokens(
    State(state): State<IdentityApiState>,
    Query(query): Query<ListTokensQuery>,
) -> ApiResult<Json<TokensResponse>> {
    let tokens = state
        .identity
        .list_tokens(query.account.as_deref(), query.include_revoked)
        .await?;
    Ok(Json(TokensResponse {
        total: tokens.len(),
        unscoped: tokens
            .iter()
            .filter(|token| token.unscoped && token.revoked_at.is_none())
            .count(),
        tokens,
    }))
}

/// DELETE /api/tokens/:id
pub async fn revoke_token(
    State(state): State<IdentityApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ApiToken>> {
    Ok(Json(state.identity.revoke_token(&id).await?))
}

fn parse_role(text: &str) -> ApiResult<Role> {
    Role::parse(text).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "{text:?} is not a role; the roles are {}",
            Role::names()
        ))
    })
}
