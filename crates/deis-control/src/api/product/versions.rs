//! Version endpoints: upload a bundle to create one, list them, and serve
//! the bundle back to nodes as a stream.

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::io::ReaderStream;
use tracing::{info, warn};

use super::ProductState;
use crate::api::error::{ApiError, ApiResult};
use crate::product::Version;

/// Version as seen from outside. `bundle_path` is deliberately absent: it
/// is a control-plane local path and no client has any use for it.
#[derive(Debug, Serialize)]
pub struct VersionView {
    pub id: String,
    pub app_id: String,
    pub number: i64,
    pub bundle_sha: String,
    pub created_at: i64,
    /// The manifest as structured JSON, parsed from the stored text.
    pub manifest: Value,
}

impl From<&Version> for VersionView {
    fn from(version: &Version) -> Self {
        // The text was parsed before the row was written, so this only
        // fails if the row was tampered with underneath us.
        let manifest = serde_json::from_str(&version.manifest).unwrap_or_else(|e| {
            warn!(
                app_id = %version.app_id,
                version_id = %version.id,
                error = %e,
                "Stored manifest text no longer parses; returning it as a raw string so the version stays listable"
            );
            Value::String(version.manifest.clone())
        });
        Self {
            id: version.id.clone(),
            app_id: version.app_id.clone(),
            number: version.number,
            bundle_sha: version.bundle_sha.clone(),
            created_at: version.created_at,
            manifest,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    /// Digest the client computed over the bundle. When present the bytes
    /// must hash to exactly this, which is how a truncated upload is
    /// caught before it can become a version.
    pub sha256: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UploadResponse {
    pub version: VersionView,
    /// False when a byte-identical bundle was already recorded, in which
    /// case the existing version is returned unchanged.
    pub created: bool,
    pub bundle_url: String,
}

#[derive(Debug, Serialize)]
pub struct VersionsResponse {
    pub versions: Vec<VersionView>,
    pub total: usize,
}

/// POST /api/apps/:app_id/versions
///
/// Body is the raw bundle zip. The upload is rejected outright - nothing
/// written, no row created - when the digest does not match, the manifest
/// does not parse, or the manifest names a different app.
pub async fn upload_version(
    State(state): State<ProductState>,
    Path(app_id): Path<String>,
    Query(query): Query<UploadQuery>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<UploadResponse>)> {
    // Fail before hashing megabytes if the app does not exist.
    state.store.get_app(&app_id).await?;

    let bundles = state.bundles.clone();
    let app_for_task = app_id.clone();
    let expected = query.sha256.clone();
    let received_bytes = body.len();

    // Hashing, unzipping and writing are blocking work; keeping them off
    // the async worker keeps heartbeats flowing during a big upload.
    let stored = tokio::task::spawn_blocking(move || {
        bundles.store(&app_for_task, &body, expected.as_deref())
    })
    .await
    .map_err(|e| {
        ApiError::InternalError(format!("bundle processing task failed to complete: {e}"))
    })??;

    let (version, created) = state
        .store
        .create_version(
            &app_id,
            &stored.sha256,
            &stored.path.to_string_lossy(),
            &stored.manifest_json,
        )
        .await?;

    info!(
        app_id = %app_id,
        version_id = %version.id,
        number = version.number,
        bundle_sha = %version.bundle_sha,
        received_bytes,
        created,
        "Bundle upload accepted"
    );

    let base = state.address.resolve(&headers);
    let bundle_url = super::bundle_url(&base, &app_id, &version.id);
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(UploadResponse {
            version: VersionView::from(&version),
            created,
            bundle_url,
        }),
    ))
}

/// GET /api/apps/:app_id/versions
pub async fn list_versions(
    State(state): State<ProductState>,
    Path(app_id): Path<String>,
) -> ApiResult<Json<VersionsResponse>> {
    let versions = state.store.list_versions(&app_id).await?;
    let views: Vec<VersionView> = versions.iter().map(VersionView::from).collect();
    Ok(Json(VersionsResponse {
        total: views.len(),
        versions: views,
    }))
}

/// GET /api/apps/:app_id/versions/:version_id
pub async fn get_version(
    State(state): State<ProductState>,
    Path((app_id, version_id)): Path<(String, String)>,
) -> ApiResult<Json<VersionView>> {
    let version = state.store.get_version(&app_id, &version_id).await?;
    Ok(Json(VersionView::from(&version)))
}

/// GET /api/apps/:app_id/versions/:version_id/bundle
///
/// Streams the stored zip. This is the address that ends up in every
/// assignment, so nodes fetch bundles here rather than receiving them
/// inside a gRPC message.
pub async fn download_bundle(
    State(state): State<ProductState>,
    Path((app_id, version_id)): Path<(String, String)>,
) -> ApiResult<Response> {
    let version = state.store.get_version(&app_id, &version_id).await?;
    let path = std::path::PathBuf::from(&version.bundle_path);

    let file = tokio::fs::File::open(&path).await.map_err(|e| {
        // The row says the bundle exists but the file does not: the node
        // asking for it cannot start this version at all.
        warn!(
            app_id = %app_id,
            version_id = %version_id,
            path = %path.display(),
            error = %e,
            "Bundle file is missing on disk; nodes cannot load this version until it is uploaded again"
        );
        ApiError::InternalError(format!(
            "bundle for version {version_id} is recorded but missing on disk"
        ))
    })?;

    let size = file
        .metadata()
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    info!(
        app_id = %app_id,
        version_id = %version_id,
        bundle_sha = %version.bundle_sha,
        bytes = size,
        "Streaming bundle to a caller"
    );

    let stream = ReaderStream::new(file);
    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/zip"),
    );
    if size > 0
        && let Ok(value) = header::HeaderValue::from_str(&size.to_string())
    {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    // The bundle is content addressed, so its digest is a perfect ETag.
    if let Ok(value) = header::HeaderValue::from_str(&format!("\"{}\"", version.bundle_sha)) {
        headers.insert(header::ETAG, value);
    }
    if let Ok(value) = header::HeaderValue::from_str(&format!(
        "attachment; filename=\"{}-{}.zip\"",
        app_id, version.number
    )) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}
