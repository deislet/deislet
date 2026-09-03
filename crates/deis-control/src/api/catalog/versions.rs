//! Version endpoints: upload a bundle to create one, list them, and serve
//! the bundle back to nodes as a stream.

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Extension, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use deis_wire::CanonicalId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::io::ReaderStream;
use tracing::{info, warn};

use super::CatalogState;
use crate::api::bindings::{BindingRefusal, check_service_bindings};
use crate::api::error::{ApiError, ApiResult};
use crate::catalog::Version;
use crate::identity::Principal;

/// Version as seen from outside. `bundle_path` is deliberately absent: it
/// is a control-plane local path and no client has any use for it.
#[derive(Debug, Serialize)]
pub struct VersionView {
    pub id: String,
    pub canonical_id: CanonicalId,
    pub app_id: String,
    pub number: i64,
    pub bundle_sha: String,
    pub created_at: i64,
    /// The manifest as structured JSON, parsed from the stored text.
    pub manifest: Value,
    /// Which day's platform this version was written against, and which
    /// behaviours that leaves on, resolved from the same stored manifest.
    ///
    /// Lifted out of `manifest` deliberately. It is already in there, but the
    /// question "why does this behave differently in production" has to be
    /// answerable without reading a bundle or picking through a manifest, and
    /// this is the field `deis versions` and `deis deploy` print. A version
    /// that declares no date is `Some` with a null date - that is a complete
    /// answer and the one an operator most needs to see.
    ///
    /// `None` means the stored manifest could not be read at all, which is the
    /// same condition that leaves `manifest` holding a raw string.
    pub compatibility: Option<crate::catalog::CompatibilityProfile>,
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
        // Parsed a second time, as a manifest rather than as free JSON,
        // because resolving the block is the shared reader's job and not this
        // view's. A version whose manifest was accepted at upload always
        // resolves; one that does not is a row written by an older build or
        // tampered with, and saying "unknown" is the only honest answer.
        let compatibility = deis_bundle::Manifest::parse(&version.manifest)
            .ok()
            .and_then(|manifest| match manifest.compatibility_profile() {
                Ok(profile) => Some(profile),
                Err(e) => {
                    warn!(
                        app_id = %version.app_id,
                        version_id = %version.id,
                        error = %e,
                        "Stored manifest declares a compatibility date or flag this build cannot resolve, so this version is listed without one; a node would refuse to run it for the same reason"
                    );
                    None
                }
            });
        Self {
            id: version.id.clone(),
            canonical_id: version.canonical_id,
            app_id: version.app_id.clone(),
            number: version.number,
            bundle_sha: version.bundle_sha.clone(),
            created_at: version.created_at,
            manifest,
            compatibility,
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
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
    Query(query): Query<UploadQuery>,
    // Who is uploading. `crate::api::authz` puts it there on every request it
    // lets through, including the anonymous caller of an install with no
    // authentication, so the default below is only ever reached by a router
    // mounted without that middleware - which is a test, and which then has no
    // credential to judge.
    principal: Option<Extension<Principal>>,
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

    // The second gate, and the only one that can be applied here: a service
    // binding in this bundle is this application claiming a right to call
    // another, and the route the upload arrived on named only this one. See
    // `crate::api::bindings` for why consent is *not* checked here and where
    // it is checked instead. Nothing has been written to the catalog yet - the
    // bundle file on disk is content-addressed and shared, so a refusal here
    // leaves no version anybody can deploy.
    let principal = principal
        .map(|Extension(principal)| principal)
        .unwrap_or_else(Principal::anonymous);
    if let Err(refusal) = check_service_bindings(&principal, &stored.manifest) {
        return Err(match &refusal {
            BindingRefusal::Unentitled {
                binding,
                callee,
                required_scope,
                held,
            } => {
                warn!(
                    app_id = %app_id,
                    bundle_sha = %stored.sha256,
                    caller = %principal.label(),
                    binding = %binding,
                    callee = %callee,
                    needed = %required_scope,
                    held = %held,
                    "Refused a bundle upload: it declares a service binding to an application \
                     this credential is not entitled to. No version was created, so nothing can \
                     be deployed with that route in it, and the refusal is in the audit log"
                );
                ApiError::Forbidden(refusal.to_string())
            }
            // Not a 403: the credential may well be entitled, and a wider one
            // would be refused the same way. It is the same shape as a
            // compatibility date this build does not know, which
            // `catalog::bundle` also answers with a 400 - the bundle asks for
            // a platform this control plane is not yet.
            BindingRefusal::Unjudgeable { binding, kind, app } => {
                warn!(
                    app_id = %app_id,
                    bundle_sha = %stored.sha256,
                    caller = %principal.label(),
                    binding = %binding,
                    kind = %kind,
                    names_app = app.as_deref().unwrap_or("-"),
                    "Refused a bundle upload: it declares a binding of a kind this control plane \
                     cannot read, so the entitlement gate could not judge whether it reaches \
                     another application. This control plane is older than the builder that \
                     produced the bundle; upgrade it to a release that knows the kind. No version \
                     was created"
                );
                ApiError::BadRequest(refusal.to_string())
            }
        });
    }
    if !stored.manifest.bindings.is_empty() {
        info!(
            app_id = %app_id,
            bundle_sha = %stored.sha256,
            bindings = %stored.manifest.bindings.describe(),
            "This version declares {} binding(s): {}",
            stored.manifest.bindings.len(),
            stored.manifest.bindings.describe()
        );
    }
    if !stored.manifest.callable_by.is_empty() {
        info!(
            app_id = %app_id,
            bundle_sha = %stored.sha256,
            callable_by = %stored.manifest.callable_by.join(", "),
            "This version agrees to be called through a service binding by: {}. A node checks \
             this list, out of this bundle, on every such call",
            stored.manifest.callable_by.join(", ")
        );
    }

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

    let base = state.deliverer.base_url(&headers);
    let bundle_url = crate::delivery::bundle_url(&base, &app_id, &version.id);
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
    State(state): State<CatalogState>,
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
    State(state): State<CatalogState>,
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
    State(state): State<CatalogState>,
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
