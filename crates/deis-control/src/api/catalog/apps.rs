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
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};
use crate::api::pagination::{ControlListPage, ListQuery, list_page};
use crate::catalog::{App, RetiredAppName};

#[derive(Debug, Deserialize)]
pub struct CreateAppRequest {
    /// App name; also the app_id every component keys off.
    pub id: String,
}

#[derive(Debug, Serialize)]
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

#[derive(Debug, Serialize)]
pub struct ReleaseRetiredAppResponse {
    pub app_id: String,
    pub released: RetiredAppName,
    /// What the caller has just accepted, spelled out: the next application
    /// created under this name is the same storage tenant as the deleted one.
    pub accepted: String,
}

/// POST /api/apps
pub async fn create_app(
    State(state): State<CatalogState>,
    Json(request): Json<CreateAppRequest>,
) -> ApiResult<(StatusCode, Json<App>)> {
    let app = state.store.create_app(request.id.trim()).await?;
    Ok((StatusCode::CREATED, Json(app)))
}

/// GET /api/apps
pub async fn list_apps(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<App>>> {
    let collection = "applications";
    let binding = "scope=global;filter=all;sort=id-asc-v1";
    let limit = query.bounded_limit()?;
    let cursor = query.position(&state.store, collection, binding).await?;
    let page = state.store.list_apps_page(limit, cursor).await?;
    let response = list_page(&state.store, collection, binding, limit, page, |app| {
        crate::catalog::PagePosition {
            sort_value: 0,
            identity: app.id.clone(),
        }
    })
    .await?;
    Ok(Json(response))
}

/// GET /api/apps/:app_id
pub async fn get_app(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
) -> ApiResult<Json<App>> {
    Ok(Json(state.store.get_app(&app_id).await?))
}

/// DELETE /api/apps/:app_id
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
pub async fn delete_app(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
) -> ApiResult<Json<DeleteAppResponse>> {
    let removed = state.store.delete_app(&app_id).await?;

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

/// GET /api/retired-apps
///
/// The names of deleted applications and the tenants whose data is the reason
/// they are held. This is the only record of what became of a deleted
/// application's data: its catalog rows are gone, and deis-store and
/// deis-queue cannot be asked what they still hold.
///
/// A row with `released_at` set is a name somebody freed again: it can be
/// created, and what it says about the data is as true as it ever was.
pub async fn list_retired_apps(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<RetiredAppName>>> {
    let collection = "retired-applications";
    let binding = "scope=global;filter=all;sort=application-id-asc-v1";
    let limit = query.bounded_limit()?;
    let cursor = query.position(&state.store, collection, binding).await?;
    let page = state.store.list_retired_names_page(limit, cursor).await?;
    let response = list_page(&state.store, collection, binding, limit, page, |retired| {
        crate::catalog::PagePosition {
            sort_value: 0,
            identity: retired.app_id.clone(),
        }
    })
    .await?;
    Ok(Json(response))
}

/// DELETE /api/retired-apps/:app_id
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
    state.store.get_app(app_id).await?;
    Ok(state.store.get_environment(app_id, env_name).await?)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        Harness, TEST_STORAGE_SECRET, harness_with_storage, json, storage_wiring,
    };

    use axum::http::StatusCode;
    use serde_json::{Value, json as body};

    async fn platform() -> Harness {
        harness_with_storage(
            true,
            vec!["node-psk".to_string()],
            storage_wiring(Some(TEST_STORAGE_SECRET)),
        )
        .await
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
        let message = refused["error"].as_str().unwrap();
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
        assert_eq!(
            after["token"], before["token"],
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
