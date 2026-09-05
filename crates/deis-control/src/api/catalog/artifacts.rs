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

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};

#[derive(Debug, Deserialize)]
pub struct ArtifactUploadQuery {
    /// Expected identity of `.edge-canon/output.json`, not the transport zip.
    pub sha256: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ArtifactUploadResponse {
    pub artifact: Value,
    pub created: bool,
}

#[derive(Debug, Serialize)]
pub struct ArtifactsResponse {
    pub artifacts: Vec<Value>,
    pub total: usize,
}

/// POST /api/apps/:app_id/artifacts
///
/// The request body is only a transport zip. Control expands it into an
/// isolated directory and executes the Edge Canon validator before either a
/// storage path or catalog record becomes visible.
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

/// GET /api/apps/:app_id/artifacts
pub async fn list_artifacts(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
) -> ApiResult<Json<ArtifactsResponse>> {
    let artifacts = state.store.list_canonical_artifacts(&app_id).await?;
    Ok(Json(ArtifactsResponse {
        total: artifacts.len(),
        artifacts: artifacts
            .into_iter()
            .map(|artifact| artifact.record)
            .collect(),
    }))
}

/// GET /api/apps/:app_id/artifacts/:artifact_id
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
