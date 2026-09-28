//! App lifecycle: create, list, inspect, delete - and what becomes of the
//! name afterwards.
//!
//! Deleting an application is the one operation here that cannot finish the
//! job. Everything the application stored lives in deis-store and deis-queue
//! under a tenant built from `(application, environment)`, and neither service
//! has an operation that drops a tenant, so the data outlives the delete. The
//! name is therefore retained rather than recycled: it stays in the catalog's
//! `retired_app_names` table until somebody releases it deliberately, and
//! until they do, nothing can be created under it and inherit that data. See
//! [`delete_app`] and [`release_retired_app`].

use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use utoipa::ToSchema;

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};
use crate::api::openapi;
use crate::api::pagination::{ControlListPage, ListQuery, list_page};
use crate::catalog::{App, RetiredAppName};
use crate::identity::Principal;

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAppRequest {
    /// App name; also the app_id every component keys off.
    pub id: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeleteAppResponse {
    pub app_id: String,
    pub removed: crate::catalog::DeletedApp,
    pub bundle_files_removed: usize,
    pub artifact_directories_removed: usize,
    /// Which nodes stopped serving the app, and which did not.
    ///
    /// A node in `failed` is still running the deleted application - its
    /// code, its secrets in memory and its queue consumers - so the caller
    /// has to see that here; nothing else will ever mention this app again.
    pub unloaded: crate::delivery::RemovalReport,
    /// What was *not* removed: the name, and the data behind it.
    ///
    /// The counters in `removed` are catalog rows. This is the stored data,
    /// which is still in deis-store and deis-queue under the tenants named
    /// here, and the record that keeps the name from being handed to the next
    /// application that asks for it.
    pub retained: RetiredAppName,
    /// What the tenants in `retained` mean, in one sentence, for a caller
    /// that is showing this to a person rather than parsing it.
    pub retained_note: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReleaseRetiredAppResponse {
    pub app_id: String,
    pub released: RetiredAppName,
    /// What the caller has just accepted, spelled out: the next application
    /// created under this name is the same storage tenant as the deleted one.
    pub accepted: String,
}

/// Create an application.
#[utoipa::path(
    post,
    path = "/api/v1/apps",
    operation_id = "createApp",
    tag = "product",
    request_body = CreateAppRequest,
    responses(
        (status = 201, description = "The application as created.", body = App),
        (status = 400, response = openapi::BadRequest),
        (status = 409, response = openapi::Conflict),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn create_app(
    State(state): State<CatalogState>,
    Json(request): Json<CreateAppRequest>,
) -> ApiResult<(StatusCode, Json<App>)> {
    // The two things this create can be refused for are both about the
    // one field the body has: the id is not a name (400, `id`), or the
    // name is taken - by a live application or by a deleted one whose data
    // still holds it (409 `already-exists`, and the message says which).
    let app = state
        .store
        .create_app(request.id.trim())
        .await
        .map_err(|error| {
            ApiError::from(error)
                .creating("application")
                .invalid_field("id")
        })?;
    Ok((StatusCode::CREATED, Json(app)))
}

/// List the applications, by id.
#[utoipa::path(
    get,
    path = "/api/v1/apps",
    operation_id = "listApps",
    tag = "product",
    params(ListQuery),
    responses(
        (status = 200, description = "One page of applications.", body = ControlListPage<App>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_apps(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<App>>> {
    let collection = "applications";
    let binding = "scope=global;filter=all;sort=id-asc-v1";
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, binding)
        .await?;
    let page = state.store.list_apps_page(limit, cursor).await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        binding,
        limit,
        page,
        |app| crate::catalog::PagePosition {
            sort_value: 0,
            identity: app.id.clone(),
            secondary: None,
        },
    )
    .await?;
    Ok(Json(response))
}

/// Read one application.
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}",
    operation_id = "getApp",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id.")),
    responses(
        (status = 200, description = "The application.", body = App),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_app(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
) -> ApiResult<Json<App>> {
    Ok(Json(state.store.get_app(&app_id).await?))
}

/// Delete an application and stop every node serving it.
///
/// Removes the app together with its versions, environments, deployment
/// history, routes and secrets, drops the bundle files those versions
/// referenced, and tells every node to stop serving it.
///
/// That last step is not optional. A node adds applications from its
/// assignment list and never removes one that has left it, so a delete that
/// only touched the database would leave every node serving the application
/// forever: still answering requests, still holding its decrypted secrets
/// in memory, still polling its queues. A delete done to revoke access has
/// to actually revoke it, so a node that cannot be reached is reported in
/// the answer rather than passed over.
///
/// What it does *not* remove is the application's stored data. That lives in
/// deis-store and deis-queue, keyed by `(application, environment)`, and this
/// control plane has no call that drops a tenant there, so the answer reports
/// the tenants that still hold it and the name is retained so that the next
/// application of that name cannot be handed it silently.
#[utoipa::path(
    delete,
    path = "/api/v1/apps/{app_id}",
    operation_id = "deleteApp",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id.")),
    responses(
        (status = 200, description = "What was removed, what was unloaded, and the name now retired.", body = DeleteAppResponse),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn delete_app(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<DeleteAppResponse>> {
    let application_id = state.store.get_app(&app_id).await?.canonical_id;
    // The caller is named on the grant revocations this delete performs.
    let removed = state
        .store
        .delete_app_as(&app_id, &super::actor_subject(&principal))
        .await?;

    // Before the bundle files: the nodes are what is still serving the
    // application, and a failure to delete a file must not stop that.
    let unloaded = state
        .deliverer
        .remove_app(&app_id, removed.environment_names.clone())
        .await;

    let mut bundle_files_removed = 0usize;
    for path in &removed.bundle_paths {
        match state.bundles.remove(std::path::Path::new(path)) {
            Ok(true) => bundle_files_removed += 1,
            Ok(false) => warn!(
                app_id = %app_id,
                path = %path,
                "Bundle file of a deleted app was already gone; the database row is removed either way"
            ),
            Err(e) => warn!(
                app_id = %app_id,
                path = %path,
                error = %e,
                "Could not delete a bundle file of a deleted app; it now occupies disk space nothing references"
            ),
        }
    }

    let mut artifact_directories_removed = 0usize;
    for path in &removed.artifact_paths {
        match state.artifacts.remove(std::path::Path::new(path)) {
            Ok(true) => artifact_directories_removed += 1,
            Ok(false) => warn!(
                app_id = %app_id,
                path = %path,
                "Canonical artifact directory of a deleted app was already gone"
            ),
            Err(e) => warn!(
                app_id = %app_id,
                path = %path,
                error = %e,
                "Could not delete a canonical artifact directory; it is now unreferenced"
            ),
        }
    }

    match state.provider_payloads.remove_application(application_id) {
        Ok(true) => info!(app_id = %app_id, "Removed the deleted app's stored provider payloads"),
        Ok(false) => {}
        Err(e) => warn!(
            app_id = %app_id,
            error = %e,
            "Could not delete the deleted app's provider payloads; they are now unreferenced"
        ),
    }

    // Best effort: an empty per-app directory is noise, but a leftover one
    // is harmless, so a failure here is not worth a warning.
    let app_dir = state.bundles.root().join(&app_id);
    if std::fs::remove_dir(&app_dir).is_ok() {
        info!(app_id = %app_id, dir = %app_dir.display(), "Removed the now empty bundle directory");
    }

    if unloaded.fully_removed() {
        info!(
            app_id = %app_id,
            bundle_files_removed,
            artifact_directories_removed,
            nodes = unloaded.unloaded.len(),
            "App deletion finished; nothing serves it any more"
        );
    } else {
        warn!(
            app_id = %app_id,
            bundle_files_removed,
            artifact_directories_removed,
            still_serving = ?unloaded.failed.iter().map(|f| f.node.as_str()).collect::<Vec<_>>(),
            "App deletion finished in the catalog only; the listed nodes were not reached and keep serving the deleted application until they are restarted"
        );
    }
    let retained = removed.retired.clone();
    Ok(Json(DeleteAppResponse {
        app_id,
        retained_note: format!(
            "{}. No part of this platform deletes it. The name is retained until it is released \
             with DELETE /api/retired-apps/{}, so nothing can be deployed onto that data by \
             accident",
            retained.describe_data(),
            retained.app_id
        ),
        retained,
        removed,
        bundle_files_removed,
        artifact_directories_removed,
        unloaded,
    }))
}

/// List the names deleted applications still hold.
///
/// The names of deleted applications and the tenants whose data is the reason
/// they are held. This is the only record of what became of a deleted
/// application's data: its catalog rows are gone, and deis-store and
/// deis-queue cannot be asked what they still hold.
///
/// A row with `released_at` set is a name somebody freed again: it can be
/// created, and what it says about the data is as true as it ever was.
#[utoipa::path(
    get,
    path = "/api/v1/retired-apps",
    operation_id = "listRetiredApps",
    tag = "product",
    params(ListQuery),
    responses(
        (status = 200, description = "One page of retired names.", body = ControlListPage<RetiredAppName>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_retired_apps(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<RetiredAppName>>> {
    let collection = "retired-applications";
    let binding = "scope=global;filter=all;sort=application-id-asc-v1";
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, binding)
        .await?;
    let page = state.store.list_retired_names_page(limit, cursor).await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        binding,
        limit,
        page,
        |retired| crate::catalog::PagePosition {
            sort_value: 0,
            identity: retired.app_id.clone(),
            secondary: None,
        },
    )
    .await?;
    Ok(Json(response))
}

/// Release a retired name so it can be created again.
///
/// Free the name again. Deliberately a second call, made by an administrator,
/// rather than something the delete could have done on its own: releasing it
/// is the decision that the next application created under this name may have
/// whatever the deleted one left in deis-store and deis-queue. Whoever makes
/// it is in the audit log, and so is this call when it is refused.
///
/// The data itself is untouched either way. An operator who wants it gone
/// purges it - `deis kv`, `deis db`, `deis blob` against those tenants while
/// the application still exists, or the tenant's files under deis-store's own
/// data directory - and then releases the name.
#[utoipa::path(
    delete,
    path = "/api/v1/retired-apps/{app_id}",
    operation_id = "releaseRetiredApp",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id.")),
    responses(
        (status = 200, description = "The name is free again.", body = ReleaseRetiredAppResponse),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn release_retired_app(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
) -> ApiResult<Json<ReleaseRetiredAppResponse>> {
    let released = state.store.release_retired_name(&app_id).await?;
    Ok(Json(ReleaseRetiredAppResponse {
        app_id: released.app_id.clone(),
        accepted: format!(
            "{} is free again. {}, and the next application created under this name is that same \
             tenant: it will read and overwrite whatever is there. Credentials follow the name \
             too - a token scoped to this application, or a storage capability token minted for \
             one of those tenants, still authorises the next one, so revoke what the deleted \
             application's owner still holds",
            released.app_id,
            released.describe_data()
        ),
        released,
    }))
}

/// Shared by every endpoint nested under an app: resolve the app and the
/// environment in one step so a bad app id never reads as a bad
/// environment name.
pub(super) async fn resolve_environment(
    state: &CatalogState,
    app_id: &str,
    env_name: &str,
) -> Result<crate::catalog::Environment, ApiError> {
    let (_, environment) = resolve_app_environment(state, app_id, env_name).await?;
    Ok(environment)
}

/// Like [`resolve_environment`], keeping the application too. The list
/// endpoints need both canonical ids to bind a cursor to its scope.
pub(super) async fn resolve_app_environment(
    state: &CatalogState,
    app_id: &str,
    env_name: &str,
) -> Result<(crate::catalog::App, crate::catalog::Environment), ApiError> {
    let app = state.store.get_app(app_id).await?;
    let environment = state.store.get_environment(app_id, env_name).await?;
    Ok((app, environment))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Harness, harness_with_storage, json, storage_wiring};

    use axum::http::StatusCode;
    use serde_json::{Value, json as body};

    /// One JSON request through the whole router, on whichever surface the
    /// path names - the mounts `api::tests` builds, which is where the
    /// structured envelope is rendered.
    async fn post(
        router: &axum::Router,
        secret: &str,
        uri: &str,
        request: Value,
    ) -> (StatusCode, Value) {
        use axum::body::{Body, to_bytes};
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use tower::ServiceExt;

        let mut http = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {secret}"))
            .body(Body::from(request.to_string()))
            .unwrap();
        http.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:40000".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = router.clone().oneshot(http).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// Creating an application twice, and creating one under a name that is
    /// not one: a v1 client reads the class off `code` and the member off
    /// `violations`, and the old flat spelling is not a route any more.
    #[tokio::test]
    async fn a_v1_create_says_already_exists_and_names_the_field_it_refused() {
        let (router, _dir, secret) = crate::api::tests::full_router().await;
        let (status, created) =
            post(&router, &secret, "/api/v1/apps", body!({ "id": "shop" })).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");

        let (status, refused) =
            post(&router, &secret, "/api/v1/apps", body!({ "id": "shop" })).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refused}");
        assert_eq!(refused["error"]["code"], "already-exists");
        assert_eq!(refused["error"]["target"]["resource"], "application");
        assert!(refused["error"]["target"].get("field").is_none());
        assert_eq!(refused["error"]["message"], "app shop already exists");
        assert_eq!(refused["error"]["retryable"], false);

        let (status, refused) = post(
            &router,
            &secret,
            "/api/v1/apps",
            body!({ "id": "Not A Name" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
        assert_eq!(refused["error"]["code"], "invalid-argument");
        let violation = &refused["error"]["violations"][0];
        assert_eq!(violation["field"], "id");
        assert_eq!(violation["code"], "invalid");
        assert_eq!(violation["message"], refused["error"]["message"]);

        // The old flat spelling is not a route of this listener any more:
        // the refusal is the route fallback's.
        let (status, refused) =
            post(&router, &secret, "/api/apps", body!({ "id": "Not A Name" })).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(refused["error"]["code"], "route-not-found", "{refused}");
        assert_eq!(refused.as_object().unwrap().len(), 1, "{refused}");
    }

    async fn platform() -> Harness {
        harness_with_storage(true, storage_wiring(true)).await
    }

    async fn app_with_production(h: &Harness, app: &str) {
        json(&h.router, "POST", "/api/apps", body!({ "id": app })).await;
        json(
            &h.router,
            "POST",
            &format!("/api/apps/{app}/environments"),
            body!({ "name": "production" }),
        )
        .await;
    }

    async fn storage_token(h: &Harness, app: &str) -> Value {
        let (status, token) = json(
            &h.router,
            "GET",
            &format!("/api/apps/{app}/environments/production/storage-token"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{token}");
        token
    }

    /// The defect, end to end. An application's kv, database, objects and
    /// queued messages live in deis-store and deis-queue under
    /// `<app>.<environment>`, and nothing here can delete them - so the next
    /// application of that name used to be that tenant, whoever created it,
    /// and read and overwrote data it had never been given.
    #[tokio::test]
    async fn the_name_of_a_deleted_application_is_not_handed_to_the_next_caller() {
        let h = platform().await;
        app_with_production(&h, "victim").await;
        let before = storage_token(&h, "victim").await;
        assert_eq!(before["tenant"], "victim.production");

        let (status, deleted) = json(&h.router, "DELETE", "/api/apps/victim", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{deleted}");
        // The answer has to say what became of the data, because nothing
        // else ever will: the rows that named this application are gone.
        assert_eq!(deleted["retained"]["environments"][0], "production");
        assert!(deleted["retained"]["released_at"].is_null());
        let note = deleted["retained_note"].as_str().unwrap();
        assert!(note.contains("victim.production"), "{note}");
        assert!(note.contains("/api/retired-apps/victim"), "{note}");

        let (status, refused) = json(&h.router, "POST", "/api/apps", body!({"id": "victim"})).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a name whose data is still out there is not free: {refused}"
        );
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(message.contains("victim.production"), "{message}");
        assert!(message.contains("/api/retired-apps/victim"), "{message}");
    }

    /// What an operator can find out afterwards, and the one call that
    /// deliberately hands the data on.
    #[tokio::test]
    async fn releasing_the_name_is_a_second_act_that_says_what_it_gives_away() {
        let h = platform().await;
        app_with_production(&h, "victim").await;
        let before = storage_token(&h, "victim").await;
        json(&h.router, "DELETE", "/api/apps/victim", Value::Null).await;

        let (status, listed) = json(&h.router, "GET", "/api/retired-apps", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        assert_eq!(listed["schemaVersion"], 1);
        assert_eq!(listed["page"]["total"], 1);
        assert_eq!(listed["items"][0]["app_id"], "victim");
        assert_eq!(listed["items"][0]["environments"][0], "production");

        let (status, released) =
            json(&h.router, "DELETE", "/api/retired-apps/victim", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{released}");
        let accepted = released["accepted"].as_str().unwrap();
        assert!(
            accepted.contains("victim.production"),
            "the caller has to be told which tenant they just handed over: {accepted}"
        );

        // And now, deliberately, the name works again - and it is the same
        // storage tenant, which is exactly what the release said it would be.
        let (status, created) = json(&h.router, "POST", "/api/apps", body!({"id": "victim"})).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        json(
            &h.router,
            "POST",
            "/api/apps/victim/environments",
            body!({ "name": "production" }),
        )
        .await;
        let after = storage_token(&h, "victim").await;
        assert_eq!(after["tenant"], before["tenant"]);

        // Both tokens must be valid Ed25519 capabilities for the same tenant.
        // They differ byte-for-byte (random jti), but authorise the same tenant
        // because releasing a name means someone else can now use it under the
        // same `<app>.<environment>` string.
        let verifying_key = h.state.issuer.verifying_key().await;
        let before_claims =
            deis_capability::decode(before["token"].as_str().unwrap(), &verifying_key)
                .expect("before token must be a valid Ed25519 capability");
        let after_claims =
            deis_capability::decode(after["token"].as_str().unwrap(), &verifying_key)
                .expect("after token must be a valid Ed25519 capability");
        assert!(
            before_claims.authorizes_tenant("victim.production"),
            "before token must authorise the tenant"
        );
        assert!(
            after_claims.authorizes_tenant("victim.production"),
            "the tenant key is (application, environment) by design; releasing the name is the \
             moment somebody accepted that"
        );
    }

    /// A name nothing retired must not answer "released": a caller that
    /// misspelled the name would believe it had cleared the way.
    #[tokio::test]
    async fn releasing_a_name_that_was_never_retired_is_a_404() {
        let h = platform().await;
        app_with_production(&h, "live").await;

        let (status, _) = json(&h.router, "DELETE", "/api/retired-apps/live", Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = json(
            &h.router,
            "DELETE",
            "/api/retired-apps/never-existed",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Releasing twice is the same question asked again, and the second
        // answer must not be "yes".
        json(&h.router, "DELETE", "/api/apps/live", Value::Null).await;
        let (first, _) = json(&h.router, "DELETE", "/api/retired-apps/live", Value::Null).await;
        assert_eq!(first, StatusCode::OK);
        let (second, _) = json(&h.router, "DELETE", "/api/retired-apps/live", Value::Null).await;
        assert_eq!(second, StatusCode::NOT_FOUND);
    }
}
