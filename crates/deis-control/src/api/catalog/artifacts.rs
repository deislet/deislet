//! Canonical artifact catalog endpoints.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
};
use deis_wire::CanonicalId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};
use utoipa::IntoParams;
use utoipa::ToSchema;

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};
use crate::api::openapi;
use crate::api::pagination::{ControlListPage, ListQuery, id_scope, list_page};
use crate::catalog::{CatalogPage, PagePosition};

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ArtifactUploadQuery {
    /// Expected identity of `.edge-canon/output.json`, not the transport zip.
    pub sha256: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactUploadResponse {
    #[schema(value_type = crate::api::openapi::CanonicalArtifactDocument)]
    pub artifact: Value,
    pub created: bool,
}

/// Upload a transport zip as a canonical artifact.
///
/// The request body is only a transport zip. Control expands it into an
/// isolated directory and executes the Edge Canon validator before either a
/// storage path or catalog record becomes visible.
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/artifacts",
    operation_id = "uploadArtifact",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ArtifactUploadQuery),
    request_body(content = openapi::BinaryBody, content_type = "application/octet-stream", description = "The canonical artifact archive."),
    responses(
        (status = 200, description = "The same artifact was already stored, byte for byte; `created` is false and nothing was written.", body = ArtifactUploadResponse),
        (status = 201, description = "The artifact was verified and stored; `created` is true.", body = ArtifactUploadResponse),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 413, response = openapi::PayloadTooLarge),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn upload_artifact(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
    Query(query): Query<ArtifactUploadQuery>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<ArtifactUploadResponse>)> {
    let app = state.store.get_app(&app_id).await?;
    let artifacts = state.artifacts.clone();
    let expected = query.sha256.clone();
    let bytes = body.to_vec();
    let stored = tokio::task::spawn_blocking(move || {
        artifacts.store_transport(app.canonical_id, &bytes, expected.as_deref())
    })
    .await
    .map_err(|error| {
        ApiError::InternalError(format!(
            "canonical artifact processing task failed to complete: {error}"
        ))
    })??;

    let was_published = stored.published;
    let (artifact, created) = match state
        .store
        .create_canonical_artifact(&app_id, &stored)
        .await
    {
        Ok(result) => result,
        Err(error) => {
            if was_published {
                warn!(
                    app_id = %app_id,
                    path = %stored.path.display(),
                    "Canonical directory was published but its catalog transaction failed; the unreferenced directory is retained for safe retry"
                );
            }
            return Err(error.into());
        }
    };
    info!(
        app_id = %app_id,
        artifact_id = %artifact.id,
        artifact_sha256 = %artifact.artifact_sha256,
        created,
        "Canonical artifact upload accepted"
    );
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(ArtifactUploadResponse {
            artifact: artifact.record,
            created,
        }),
    ))
}

/// List an application's canonical artifacts.
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/artifacts",
    operation_id = "listArtifacts",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ListQuery),
    responses(
        (status = 200, description = "One page of canonical artifact records.", body = ControlListPage<openapi::CanonicalArtifactDocument>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_artifacts(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<Value>>> {
    let app = state.store.get_app(&app_id).await?;
    let collection = "application-artifacts";
    let binding = format!(
        "{};filter=all;sort=created-at-desc,artifact-id-desc-v1",
        id_scope("application", app.canonical_id)
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .store
        .list_canonical_artifacts_page(&app_id, limit, cursor)
        .await?;
    let next_position = page.items.last().map(|artifact| PagePosition {
        sort_value: artifact.created_at,
        identity: artifact.id.to_string(),
        secondary: None,
    });
    let page = CatalogPage {
        items: page
            .items
            .into_iter()
            .map(|artifact| artifact.record)
            .collect(),
        total: page.total,
        has_more: page.has_more,
    };
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        move |_| next_position.expect("a page with a successor has a last artifact"),
    )
    .await?;
    Ok(Json(response))
}

/// Read one canonical artifact.
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/artifacts/{artifact_id}",
    operation_id = "getArtifact",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("artifact_id" = String, Path, description = "The canonical artifact's UUIDv7.")),
    responses(
        (status = 200, description = "The canonical artifact record.", body = openapi::CanonicalArtifactDocument),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_artifact(
    State(state): State<CatalogState>,
    Path((app_id, artifact_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let artifact_id = artifact_id.parse::<CanonicalId>().map_err(|error| {
        ApiError::BadRequest(format!("canonical artifact id is invalid: {error}"))
    })?;
    let artifact = state
        .store
        .get_canonical_artifact(&app_id, artifact_id)
        .await?;
    Ok(Json(artifact.record))
}

/// The stored identity of a driver-deployed provider payload: exactly what
/// `POST …/provider-deployments` names in its `artifact`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPayloadUploadResponse {
    /// The canonical parent this payload was derived from.
    #[schema(value_type = String, format = Uuid)]
    pub artifact_id: CanonicalId,
    /// `cloudflare`.
    pub provider: String,
    pub standard_version: String,
    /// Identity of the derivation record.
    pub manifest_sha256: String,
    /// File-set digest of the payload tree; a provider deployment's
    /// `artifact.providerPayloadSha256`.
    pub payload_root_sha256: String,
    /// False when this exact derivation was already recorded.
    pub created: bool,
}

/// Upload a payload that an external provider driver deploys.
///
/// The body is a zip envelope with `.deislet/provider-derivation.json` and
/// the payload tree under `payload/` (for Cloudflare: `wrangler.toml`, the
/// Worker bundle and `binding-plan.json`). Control revalidates the stored
/// canonical parent, the derivation record and the payload's file-set digest,
/// stores the tree under that digest and records the derivation. When a
/// provider deployment names the digest, the Control copies this tree into
/// the driver's per-operation workspace and verifies it again there.
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/artifacts/{artifact_id}/provider-payloads",
    operation_id = "uploadProviderPayload",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("artifact_id" = String, Path, description = "The canonical artifact's UUIDv7.")),
    request_body(content = openapi::BinaryBody, content_type = "application/octet-stream", description = "The provider payload envelope."),
    responses(
        (status = 200, description = "This exact derivation was already recorded; `created` is false and nothing was written.", body = ProviderPayloadUploadResponse),
        (status = 201, description = "The payload was verified and stored.", body = ProviderPayloadUploadResponse),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 413, response = openapi::PayloadTooLarge),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn upload_provider_payload(
    State(state): State<CatalogState>,
    Path((app_id, artifact_id)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<ProviderPayloadUploadResponse>)> {
    let artifact_id = artifact_id.parse::<CanonicalId>().map_err(|error| {
        ApiError::BadRequest(format!("canonical artifact id is invalid: {error}"))
    })?;
    let artifact = state
        .store
        .get_canonical_artifact(&app_id, artifact_id)
        .await?;
    let app = state.store.get_app(&app_id).await?;
    if std::path::Path::new(&artifact.storage_path)
        != state
            .artifacts
            .path_for(app.canonical_id, &artifact.artifact_sha256)
    {
        return Err(ApiError::InternalError(
            "canonical artifact catalog path is outside its application store".into(),
        ));
    }
    let payloads = state.provider_payloads.clone();
    let bytes = body.to_vec();
    let stored = tokio::task::spawn_blocking(move || {
        payloads.store_envelope(app.canonical_id, &artifact, &bytes)
    })
    .await
    .map_err(|error| {
        ApiError::InternalError(format!(
            "provider payload processing task failed to complete: {error}"
        ))
    })??;
    let (derivation, created) = state
        .store
        .record_provider_derivation(&app_id, artifact_id, stored.spec)
        .await?;
    info!(
        app_id = %app_id,
        artifact_id = %artifact_id,
        provider = %derivation.provider,
        payload_root_sha256 = %derivation.payload_root_sha256,
        created,
        "Provider payload upload accepted"
    );
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(ProviderPayloadUploadResponse {
            artifact_id,
            provider: derivation.provider,
            standard_version: derivation.standard_version,
            manifest_sha256: derivation.manifest_sha256,
            payload_root_sha256: derivation.payload_root_sha256,
            created,
        }),
    ))
}
