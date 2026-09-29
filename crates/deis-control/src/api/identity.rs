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
use utoipa::{IntoParams, ToSchema};

use super::openapi;
use crate::api::error::{ApiError, ApiResult};
use crate::api::pagination::{ControlListPage, ListQuery, list_page};
use crate::catalog::PagePosition;
use crate::identity::{
    ApiToken, Authority, IdentityStore, Role, Scope,
    store::{Account, TokenListFilter, TokenScopeFilter},
};

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

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAccountRequest {
    pub name: String,
    /// One of the names in [`Role`]. Required rather than defaulted: the
    /// difference between `viewer` and `owner` is the whole point, and a
    /// default would be the one somebody forgot to think about.
    pub role: String,
}

/// List the accounts, oldest first.
///
/// The unified list envelope, oldest account first. There is one scope
/// (the whole installation) and no filter, so the cursor binding only has
/// to name the sort.
#[utoipa::path(
    get,
    path = "/api/v1/accounts",
    operation_id = "listAccounts",
    tag = "product",
    params(ListQuery),
    responses(
        (status = 200, description = "One page of accounts.", body = ControlListPage<Account>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_accounts(
    State(state): State<IdentityApiState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<Account>>> {
    let collection = "accounts";
    let binding = "scope=global;filter=all;sort=created-at-asc,name-asc-v1";
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.identity.page_cursors(), collection, binding)
        .await?;
    let page = state.identity.list_accounts_page(limit, cursor).await?;
    let response = list_page(
        &state.identity.page_cursors(),
        collection,
        binding,
        limit,
        page,
        |account| PagePosition {
            sort_value: account.created_at,
            identity: account.name.clone(),
            secondary: None,
        },
    )
    .await?;
    Ok(Json(response))
}

/// Create an account.
#[utoipa::path(
    post,
    path = "/api/v1/accounts",
    operation_id = "createAccount",
    tag = "product",
    request_body = CreateAccountRequest,
    responses(
        (status = 200, description = "The account as created.", body = Account),
        (status = 400, response = openapi::BadRequest),
        (status = 409, response = openapi::Conflict),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn create_account(
    State(state): State<IdentityApiState>,
    Json(request): Json<CreateAccountRequest>,
) -> ApiResult<Json<Account>> {
    let role = parse_role(&request.role)?;
    // `name` is the only member the store validates here, and the only
    // clash it can report is an account of that name.
    let account = state
        .identity
        .create_account(&request.name, role)
        .await
        .map_err(|error| {
            ApiError::from(error)
                .creating("account")
                .invalid_field("name")
        })?;
    Ok(Json(account))
}

/// Read one account.
#[utoipa::path(
    get,
    path = "/api/v1/accounts/{account}",
    operation_id = "getAccount",
    tag = "product",
    params(("account" = String, Path, description = "The account's name.")),
    responses(
        (status = 200, description = "The account.", body = Account),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_account(
    State(state): State<IdentityApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Account>> {
    Ok(Json(state.identity.get_account(&name).await?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateAccountRequest {
    /// Absent means "leave it".
    #[serde(default)]
    pub role: Option<String>,
    /// Absent means "leave it". `true` refuses every token of this account
    /// without revoking any of them.
    #[serde(default)]
    pub disabled: Option<bool>,
}

/// Change an account's role, or disable it.
#[utoipa::path(
    patch,
    path = "/api/v1/accounts/{account}",
    operation_id = "updateAccount",
    tag = "product",
    params(("account" = String, Path, description = "The account's name.")),
    request_body = UpdateAccountRequest,
    responses(
        (status = 200, description = "The account after the change.", body = Account),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
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

#[derive(Debug, Deserialize, ToSchema)]
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
    /// Ask for a credential subject to no scope at all - everything its
    /// account's role permits. Spelled out rather than implied by an empty
    /// scope list, because "everything" and "nothing" must not be one typo
    /// apart.
    #[serde(default)]
    pub unscoped: bool,
}

/// What creating a token answers. The only place a token's secret is ever
/// serialised.
#[derive(Debug, Serialize, ToSchema)]
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

/// Mint a token for an account.
#[utoipa::path(
    post,
    path = "/api/v1/accounts/{account}/tokens",
    operation_id = "createToken",
    tag = "product",
    params(("account" = String, Path, description = "The account the token belongs to.")),
    request_body = CreateTokenRequest,
    responses(
        (status = 200, description = "The token and, once only, its secret.", body = MintResponse),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn create_token(
    State(state): State<IdentityApiState>,
    Path(account): Path<String>,
    Json(request): Json<CreateTokenRequest>,
) -> ApiResult<Json<MintResponse>> {
    let authority = match (request.unscoped, request.scopes) {
        (true, Some(scopes)) if !scopes.is_empty() => {
            return Err(ApiError::BadRequest(
                "a token is either unscoped or carries scopes, not both: an unscoped credential is \
                 an unscoped credential and a scope list would say nothing about it"
                    .to_string(),
            ));
        }
        (true, _) => Authority::Unscoped,
        (false, None) => {
            let account = state.identity.get_account(&account).await?;
            Authority::Scoped(account.role.default_scopes())
        }
        // Which scope was rejected, not just that one was: the list is
        // the caller's own and a token request often carries several.
        (false, Some(scopes)) => Authority::Scoped(
            scopes
                .iter()
                .enumerate()
                .map(|(index, text)| {
                    Scope::parse(text).map_err(|e| {
                        ApiError::BadRequest(e.to_string())
                            .invalid_field(&format!("scopes[{index}]"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
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

/// What `GET /api/tokens` accepts: the page controls of every list route
/// plus the three filters a token listing combines with "and".
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct TokenListQuery {
    /// Page size, `1..=1000`; absent means 100.
    #[param(minimum = 1, maximum = 1000)]
    pub limit: Option<usize>,
    /// The `page.nextCursor` of the previous answer. Opaque, bound to the
    /// collection and the filters it was issued for, and refused with 400
    /// `cursor-invalid` anywhere else.
    pub cursor: Option<String>,
    /// An account's name. Resolved to its id before anything is read, so an
    /// unknown name is a 404 rather than an empty list.
    #[serde(default)]
    pub account: Option<String>,
    /// Include credentials that no longer work. Off by default so a listing
    /// answers "what can reach this platform right now".
    #[serde(default)]
    pub include_revoked: bool,
    /// `all` (the default) or `unscoped`. Under `unscoped` the page total
    /// is the number of credentials that answer to no scope, which is what
    /// `deis token list` warns about; a caller that only wants the number
    /// asks for `scope=unscoped&limit=1` and reads `page.total`.
    #[serde(default)]
    pub scope: Option<String>,
}

impl TokenListQuery {
    fn page_query(&self) -> ListQuery {
        ListQuery {
            limit: self.limit,
            cursor: self.cursor.clone(),
        }
    }

    fn scope_filter(&self) -> ApiResult<TokenScopeFilter> {
        match self.scope.as_deref().unwrap_or("all") {
            "all" => Ok(TokenScopeFilter::All),
            "unscoped" => Ok(TokenScopeFilter::Unscoped),
            scope => Err(ApiError::BadRequest(format!(
                "token scope filter must be all or unscoped; got {scope}"
            ))),
        }
    }
}

/// List the credentials, oldest first.
///
/// The unified list envelope, oldest credential first. Every filter is part
/// of the cursor binding - by the account's id, never its name - so a
/// cursor issued for one listing cannot be resumed under another.
#[utoipa::path(
    get,
    path = "/api/v1/tokens",
    operation_id = "listTokens",
    tag = "product",
    params(TokenListQuery),
    responses(
        (status = 200, description = "One page of tokens.", body = ControlListPage<ApiToken>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_tokens(
    State(state): State<IdentityApiState>,
    Query(query): Query<TokenListQuery>,
) -> ApiResult<Json<ControlListPage<ApiToken>>> {
    let account_id = match query.account.as_deref() {
        Some(name) => Some(state.identity.get_account(name).await?.id),
        None => None,
    };
    let scope = query.scope_filter()?;
    let collection = "api-tokens";
    let binding = format!(
        "scope=global;filter=account:{},revoked:{},scope:{};sort=created-at-asc,id-asc-v1",
        account_id.as_deref().unwrap_or("all"),
        if query.include_revoked {
            "included"
        } else {
            "excluded"
        },
        scope.as_str()
    );
    let page_query = query.page_query();
    let limit = page_query.bounded_limit()?;
    let cursor = page_query
        .position(&state.identity.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .identity
        .list_tokens_page(
            TokenListFilter {
                account_id,
                include_revoked: query.include_revoked,
                scope,
            },
            limit,
            cursor,
        )
        .await?;
    let response = list_page(
        &state.identity.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |token| PagePosition {
            sort_value: token.created_at,
            identity: token.id.clone(),
            secondary: None,
        },
    )
    .await?;
    Ok(Json(response))
}

/// Revoke a token.
#[utoipa::path(
    delete,
    path = "/api/v1/tokens/{id}",
    operation_id = "revokeToken",
    tag = "product",
    params(("id" = String, Path, description = "The token's id.")),
    responses(
        (status = 200, description = "The token, now revoked.", body = ApiToken),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn revoke_token(
    State(state): State<IdentityApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ApiToken>> {
    Ok(Json(state.identity.revoke_token(&id).await?))
}

/// The `role` member of a create or a change, which is a body field of
/// that name in both.
fn parse_role(text: &str) -> ApiResult<Role> {
    Role::parse(text).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "{text:?} is not a role; the roles are {}",
            Role::names()
        ))
        .invalid_field("role")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::catalog::tests::json;
    use crate::database::Database;
    use axum::{
        Router,
        http::StatusCode,
        routing::{delete, get, post},
    };
    use rusqlite::params;
    use serde_json::Value;

    async fn harness() -> (Router, Arc<IdentityStore>, Arc<Database>) {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let identity = Arc::new(IdentityStore::new(Arc::clone(&database)));
        let router = Router::new()
            .route("/api/accounts", get(list_accounts).post(create_account))
            .route("/api/accounts/:name/tokens", post(create_token))
            .route("/api/tokens", get(list_tokens))
            .route("/api/tokens/:id", delete(revoke_token))
            .with_state(IdentityApiState {
                identity: Arc::clone(&identity),
            });
        (router, identity, database)
    }

    /// Accounts with a chosen `created_at`, so the tie-breaker is exercised
    /// on purpose rather than left to whether the clock ticked mid-test.
    async fn insert_account(database: &Database, name: &str, created_at: i64) {
        let name = name.to_string();
        database
            .with_connection(move |conn| {
                conn.execute(
                    "INSERT INTO accounts (id, name, role, created_at, disabled_at)
                     VALUES (?1, ?2, 'viewer', ?3, NULL)",
                    params![format!("id-{name}"), name, created_at],
                )?;
                Ok::<_, crate::database::DatabaseError>(())
            })
            .await
            .unwrap();
    }

    fn names(page: &Value) -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap().to_string())
            .collect()
    }

    fn ids(page: &Value) -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap().to_string())
            .collect()
    }

    /// The two refusals a `POST /accounts` can carry, over the whole
    /// router so the envelope is the one a client reads: a name that is
    /// taken answers `already-exists` about the account, and a role that is
    /// not a role names the member it came from.
    #[tokio::test]
    async fn creating_an_account_names_the_clash_and_the_field_on_v1() {
        use axum::body::{Body, to_bytes};
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use tower::ServiceExt;

        let (router, _dir, secret) = crate::api::tests::full_router().await;
        let secret = &secret;
        let post = |body: Value| {
            let router = router.clone();
            async move {
                let mut request = Request::builder()
                    .method("POST")
                    .uri("/api/v1/accounts")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {secret}"))
                    .body(Body::from(body.to_string()))
                    .unwrap();
                request.extensions_mut().insert(ConnectInfo(
                    "127.0.0.1:40000".parse::<std::net::SocketAddr>().unwrap(),
                ));
                let response = router.oneshot(request).await.unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                (status, serde_json::from_slice::<Value>(&bytes).unwrap())
            }
        };

        let (status, created) = post(serde_json::json!({ "name": "alice", "role": "owner" })).await;
        assert_eq!(status, StatusCode::OK, "{created}");

        let (status, refused) = post(serde_json::json!({ "name": "alice", "role": "owner" })).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refused}");
        assert_eq!(refused["error"]["code"], "already-exists");
        assert_eq!(refused["error"]["target"]["resource"], "account");
        assert_eq!(
            refused["error"]["message"],
            "an account named alice already exists"
        );

        let (status, refused) =
            post(serde_json::json!({ "name": "bob", "role": "sysadmin" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
        assert_eq!(refused["error"]["code"], "invalid-argument");
        assert_eq!(refused["error"]["violations"][0]["field"], "role");
        assert_eq!(refused["error"]["violations"][0]["code"], "invalid");
    }

    #[tokio::test]
    async fn accounts_page_through_the_unified_envelope_oldest_first() {
        let (router, _, database) = harness().await;
        insert_account(&database, "zulu", 5).await;
        insert_account(&database, "bravo", 10).await;
        insert_account(&database, "alpha", 10).await;

        let (status, first) = json(&router, "GET", "/api/accounts?limit=2", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["schemaVersion"], 1);
        assert_eq!(first["page"]["limit"], 2);
        assert_eq!(first["page"]["count"], 2);
        assert_eq!(first["page"]["total"], 3);
        assert_eq!(names(&first), ["zulu", "alpha"]);
        let cursor = first["page"]["nextCursor"].as_str().unwrap().to_string();
        assert_eq!(cursor.len(), 43);

        // The page size may change between pages; the position may not.
        let (status, second) = json(
            &router,
            "GET",
            &format!("/api/accounts?limit=1&cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(names(&second), ["bravo"]);
        assert_eq!(second["page"]["total"], 3);
        assert!(second["page"]["nextCursor"].is_null());

        // An accounts cursor opens no other collection.
        let (status, _) = json(
            &router,
            "GET",
            &format!("/api/tokens?cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = json(&router, "GET", "/api/accounts?limit=0", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(&router, "GET", "/api/accounts?limit=1001", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn tokens_page_through_the_unified_envelope_and_count_the_unscoped_ones() {
        let (router, identity, _) = harness().await;
        identity.create_account("ci", Role::Deployer).await.unwrap();
        identity.create_account("ops", Role::Owner).await.unwrap();
        for (account, name, authority) in [
            (
                "ci",
                "preview",
                Authority::Scoped(Role::Deployer.default_scopes()),
            ),
            ("ops", "laptop", Authority::Unscoped),
            (
                "ci",
                "release",
                Authority::Scoped(Role::Deployer.default_scopes()),
            ),
            ("ops", "old-root", Authority::Unscoped),
        ] {
            identity
                .create_token(account, name, authority)
                .await
                .unwrap();
        }
        let (status, all) = json(&router, "GET", "/api/tokens", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{all}");
        assert_eq!(all["page"]["total"], 4);
        assert_eq!(all["page"]["count"], 4);
        assert!(all["page"]["nextCursor"].is_null());
        let everything = ids(&all);
        // Same-second tokens tie-break on their random id, so the one to
        // revoke is found by name, not by position.
        let old_root = all["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|token| token["name"] == "old-root")
            .map(|token| token["id"].as_str().unwrap().to_string())
            .unwrap();
        let live: Vec<String> = everything
            .iter()
            .filter(|id| **id != old_root)
            .cloned()
            .collect();

        // Revoking one drops it from the default listing and its total.
        let (status, _) = json(
            &router,
            "DELETE",
            &format!("/api/tokens/{old_root}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, first) = json(&router, "GET", "/api/tokens?limit=2", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["page"]["total"], 3);
        assert_eq!(first["page"]["count"], 2);
        assert_eq!(ids(&first), live[..2]);
        let cursor = first["page"]["nextCursor"].as_str().unwrap().to_string();

        let (status, second) = json(
            &router,
            "GET",
            &format!("/api/tokens?limit=5&cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(ids(&second), live[2..3]);
        assert_eq!(second["page"]["total"], 3);
        assert!(second["page"]["nextCursor"].is_null());
        assert!(
            !second.to_string().contains("secret"),
            "a token listing must never carry a credential"
        );

        // The same cursor under any other filter is refused: the scope
        // filter, the revoked filter and the account filter each change
        // the binding.
        for uri in [
            format!("/api/tokens?scope=unscoped&cursor={cursor}"),
            format!("/api/tokens?include_revoked=true&cursor={cursor}"),
            format!("/api/tokens?account=ci&cursor={cursor}"),
            format!("/api/accounts?cursor={cursor}"),
        ] {
            let (status, _) = json(&router, "GET", &uri, Value::Null).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }

        // The unscoped count is a filtered total, one live root token now.
        let (status, unscoped) = json(
            &router,
            "GET",
            "/api/tokens?scope=unscoped&limit=1",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{unscoped}");
        assert_eq!(unscoped["page"]["total"], 1);
        assert_eq!(unscoped["items"][0]["name"], "laptop");
        let (_, unscoped_ever) = json(
            &router,
            "GET",
            "/api/tokens?scope=unscoped&include_revoked=true&limit=1",
            Value::Null,
        )
        .await;
        assert_eq!(unscoped_ever["page"]["total"], 2);

        // Per account, by the id the binding carries; an unknown name is
        // an error, not an empty page.
        let (status, ci) = json(&router, "GET", "/api/tokens?account=ci", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{ci}");
        assert_eq!(ci["page"]["total"], 2);
        let (status, _) = json(&router, "GET", "/api/tokens?account=nobody", Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = json(&router, "GET", "/api/tokens?scope=root", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(&router, "GET", "/api/tokens?before=1", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
