//! Version endpoints: upload a bundle to create one, list them, and serve
//! the bundle back to nodes as a stream.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path as FsPath, PathBuf};

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
use utoipa::IntoParams;
use utoipa::ToSchema;

use super::CatalogState;
use crate::api::bindings::{BindingRefusal, check_service_bindings};
use crate::api::error::{ApiError, ApiResult};
use crate::api::openapi;
use crate::api::pagination::{ControlListPage, ListQuery, id_scope, list_page};
use crate::catalog::{CatalogPage, PagePosition, ProviderDerivationSpec, StoredBundle, Version};
use crate::identity::{Principal, PrincipalKind};

/// Version as seen from outside. `bundle_path` is deliberately absent: it
/// is a control-plane local path and no client has any use for it.
#[derive(Debug, Serialize, ToSchema)]
pub struct VersionView {
    pub id: String,
    #[schema(value_type = String, format = Uuid)]
    pub canonical_id: CanonicalId,
    pub app_id: String,
    pub number: i64,
    pub bundle_sha: String,
    /// Exact provider-neutral parent and provider derivation. Both are absent
    /// for a native bundle uploaded directly, without a canonical artifact:
    /// such a version runs on Deislet nodes and cannot be derived for another
    /// provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = Uuid, nullable = false)]
    pub canonical_artifact_id: Option<CanonicalId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub provider_derivation_sha256: Option<String>,
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
    #[schema(value_type = Option<crate::api::openapi::CompatibilityProfileDocument>)]
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
            canonical_artifact_id: version.canonical_artifact_id,
            provider_derivation_sha256: version.provider_derivation_sha256.clone(),
            created_at: version.created_at,
            manifest,
            compatibility,
        }
    }
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UploadQuery {
    /// Digest the client computed over the bundle. When present the bytes
    /// must hash to exactly this, which is how a truncated upload is
    /// caught before it can become a version.
    pub sha256: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UploadResponse {
    pub version: VersionView,
    /// False when a byte-identical bundle was already recorded, in which
    /// case the existing version is returned unchanged.
    pub created: bool,
    pub bundle_url: String,
}

/// The provider upload envelope has a small derivation record beside the
/// native bundle. The HTTP body itself remains bounded by the route.
pub const MAX_PROVIDER_ENVELOPE_BYTES: usize = super::MAX_BUNDLE_BYTES + 4 * 1024 * 1024;
const DERIVATION_ENTRY: &str = ".deislet/provider-derivation.json";
const BUNDLE_ENTRY: &str = "payload/bundle.zip";

/// Upload a bundle as a new version.
///
/// Body is the raw bundle zip. The upload is rejected outright - nothing
/// written, no row created - when the digest does not match, the manifest
/// does not parse, or the manifest names a different app.
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/versions",
    operation_id = "uploadVersion",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), UploadQuery),
    request_body(content = openapi::BinaryBody, content_type = "application/octet-stream", description = "The bundle bytes."),
    responses(
        (status = 200, description = "A byte-identical bundle was already stored; `created` is false and nothing was written.", body = UploadResponse),
        (status = 201, description = "The version was stored; `created` is true.", body = UploadResponse),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 413, response = openapi::PayloadTooLarge),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
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

    record_stored_bundle(
        &state,
        &app_id,
        principal,
        &headers,
        received_bytes,
        stored,
        None,
    )
    .await
}

/// Upload a provider-derived version bound to its canonical artifact.
///
/// The envelope contains exactly an immutable provider derivation record and
/// the Deislet native bundle it binds. Control revalidates the stored
/// canonical parent, the derivation, the payload root and the native bundle
/// before creating one lineage-bound version row.
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/artifacts/{artifact_id}/versions",
    operation_id = "uploadVersionWithLineage",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("artifact_id" = String, Path, description = "The canonical artifact's UUIDv7."), UploadQuery),
    request_body(content = openapi::BinaryBody, content_type = "application/octet-stream", description = "The provider derivation envelope."),
    responses(
        (status = 200, description = "A byte-identical envelope was already stored under this lineage; `created` is false and nothing was written.", body = UploadResponse),
        (status = 201, description = "The version was stored with its lineage to the canonical artifact.", body = UploadResponse),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 413, response = openapi::PayloadTooLarge),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn upload_version_with_lineage(
    State(state): State<CatalogState>,
    Path((app_id, artifact_id)): Path<(String, String)>,
    Query(query): Query<UploadQuery>,
    principal: Option<Extension<Principal>>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<UploadResponse>)> {
    let artifact_id = artifact_id.parse::<CanonicalId>().map_err(|error| {
        ApiError::BadRequest(format!("canonical artifact id is invalid: {error}"))
    })?;
    let artifact = state
        .store
        .get_canonical_artifact(&app_id, artifact_id)
        .await?;
    let app = state.store.get_app(&app_id).await?;
    let expected_artifact_path = state
        .artifacts
        .path_for(app.canonical_id, &artifact.artifact_sha256);
    if FsPath::new(&artifact.storage_path) != expected_artifact_path {
        return Err(ApiError::InternalError(
            "canonical artifact catalog path is outside its application store".into(),
        ));
    }
    let bundles = state.bundles.clone();
    let app_for_task = app_id.clone();
    let expected_bundle = query.sha256.clone();
    let envelope = body.to_vec();
    let received_bytes = envelope.len();
    let (stored, derivation) = tokio::task::spawn_blocking(move || {
        let derivation = validate_deislet_envelope(&artifact, &envelope)?;
        let stored = bundles.store(
            &app_for_task,
            &derivation.bundle,
            expected_bundle.as_deref(),
        )?;
        Ok::<_, crate::catalog::CatalogError>((stored, derivation.spec))
    })
    .await
    .map_err(|error| {
        ApiError::InternalError(format!(
            "lineage-bound bundle processing task failed to complete: {error}"
        ))
    })??;

    record_stored_bundle(
        &state,
        &app_id,
        principal,
        &headers,
        received_bytes,
        stored,
        Some((artifact_id, derivation)),
    )
    .await
}

async fn record_stored_bundle(
    state: &CatalogState,
    app_id: &str,
    principal: Option<Extension<Principal>>,
    headers: &axum::http::HeaderMap,
    received_bytes: usize,
    stored: StoredBundle,
    lineage: Option<(CanonicalId, ProviderDerivationSpec)>,
) -> ApiResult<(StatusCode, Json<UploadResponse>)> {
    // The second gate, and the only one that can be applied here: a service
    // binding in this bundle is this application claiming a right to call
    // another, and the route the upload arrived on named only this one. See
    // `crate::api::bindings` for why consent is *not* checked here and where
    // it is checked instead. Nothing has been written to the catalog yet - the
    // bundle file on disk is content-addressed and shared, so a refusal here
    // leaves no version anybody can deploy.
    // The gate always leaves a principal on an admitted request, so this
    // extension is present in production. If a request ever reaches this
    // handler without one - a router mounted without the gate - the binding
    // consent check runs against a principal with no authority at all and
    // fails closed: a service binding is a claim of authority over another
    // application, and "nobody here was admitted" is not a claimant.
    let principal = principal
        .map(|Extension(principal)| principal)
        .unwrap_or_else(|| Principal {
            kind: PrincipalKind::Account,
            account: None,
            role: None,
            credential_id: None,
            authority: crate::identity::Authority::Scoped(Vec::new()),
        });
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

    let (version, created) = match lineage {
        Some((artifact_id, derivation)) => {
            state
                .store
                .create_version_with_lineage(
                    app_id,
                    &stored.sha256,
                    &stored.path.to_string_lossy(),
                    &stored.manifest_json,
                    artifact_id,
                    derivation,
                )
                .await?
        }
        None => {
            state
                .store
                .create_version(
                    app_id,
                    &stored.sha256,
                    &stored.path.to_string_lossy(),
                    &stored.manifest_json,
                )
                .await?
        }
    };

    info!(
        app_id = %app_id,
        version_id = %version.id,
        number = version.number,
        bundle_sha = %version.bundle_sha,
        received_bytes,
        created,
        "Bundle upload accepted"
    );

    // The one bundle link in this crate built for a person rather than a
    // node: it goes back to whoever just uploaded, so it is spelled on the
    // listener their client is already talking to.
    let base = state.deliverer.human_base_url(headers);
    let bundle_url = crate::delivery::bundle_url(&base, app_id, &version.id);
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

#[derive(Debug)]
struct ValidatedEnvelope {
    bundle: Vec<u8>,
    spec: ProviderDerivationSpec,
}

fn validate_deislet_envelope(
    artifact: &crate::catalog::CanonicalArtifact,
    bytes: &[u8],
) -> crate::catalog::CatalogResult<ValidatedEnvelope> {
    if bytes.is_empty() || bytes.len() > MAX_PROVIDER_ENVELOPE_BYTES {
        return Err(crate::catalog::CatalogError::invalid(format!(
            "provider envelope must contain 1 to {MAX_PROVIDER_ENVELOPE_BYTES} bytes"
        )));
    }

    // Re-read the parent immediately before accepting its child. A database
    // row is not proof that the directory survived disk damage or tampering.
    let parent = deis_artifact::canonical_artifact::validate(
        FsPath::new(&artifact.storage_path),
        Some(deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION),
        &[],
    )
    .map_err(|error| {
        crate::catalog::CatalogError::BundleStorage(format!(
            "stored canonical parent failed deployment-time validation: {error}"
        ))
    })?;
    if parent.artifact_sha256 != artifact.artifact_sha256
        || parent.content_root_sha256 != artifact.content_root_sha256
        || parent.standard_version != artifact.standard_version
    {
        return Err(crate::catalog::CatalogError::BundleStorage(
            "stored canonical parent differs from its immutable catalog record".into(),
        ));
    }

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|error| {
        crate::catalog::CatalogError::invalid(format!(
            "provider envelope is not a readable zip: {error}"
        ))
    })?;
    let mut derivation = None;
    let mut bundle = None;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            crate::catalog::CatalogError::invalid(format!(
                "provider envelope entry is invalid: {error}"
            ))
        })?;
        if entry.is_dir() {
            if !matches!(entry.name(), ".deislet/" | "payload/") {
                return Err(crate::catalog::CatalogError::invalid(
                    "provider envelope contains an unexpected directory",
                ));
            }
            continue;
        }
        let limit = match entry.name() {
            DERIVATION_ENTRY => 4 * 1024 * 1024,
            BUNDLE_ENTRY => super::MAX_BUNDLE_BYTES,
            _ => {
                return Err(crate::catalog::CatalogError::invalid(
                    "provider envelope contains an unexpected file",
                ));
            }
        };
        let kind = entry.unix_mode().unwrap_or(0) & 0o170000;
        if !matches!(kind, 0 | 0o100000) || entry.size() > limit as u64 {
            return Err(crate::catalog::CatalogError::invalid(
                "provider envelope entry is not one bounded regular file",
            ));
        }
        let mut content = Vec::with_capacity(entry.size() as usize);
        entry
            .by_ref()
            .take(limit as u64 + 1)
            .read_to_end(&mut content)
            .map_err(|error| {
                crate::catalog::CatalogError::invalid(format!(
                    "provider envelope entry could not be read: {error}"
                ))
            })?;
        if content.len() > limit || content.len() as u64 != entry.size() {
            return Err(crate::catalog::CatalogError::invalid(
                "provider envelope entry exceeds its declared bound",
            ));
        }
        match entry.name() {
            DERIVATION_ENTRY => {
                if derivation.replace(content).is_some() {
                    return Err(crate::catalog::CatalogError::invalid(
                        "provider envelope repeats its derivation record",
                    ));
                }
            }
            BUNDLE_ENTRY => {
                if bundle.replace(content).is_some() {
                    return Err(crate::catalog::CatalogError::invalid(
                        "provider envelope repeats its native bundle",
                    ));
                }
            }
            _ => unreachable!("entry names were checked above"),
        }
    }
    let derivation = derivation.ok_or_else(|| {
        crate::catalog::CatalogError::invalid("provider envelope has no derivation record")
    })?;
    let bundle = bundle.ok_or_else(|| {
        crate::catalog::CatalogError::invalid("provider envelope has no native bundle")
    })?;

    let derivation_sha256 = deis_artifact::canonical_artifact::sha256(&derivation);
    let declared: deis_artifact::provider_derivation::ProviderDerivationManifest =
        serde_json::from_slice(&derivation).map_err(|_| {
            crate::catalog::CatalogError::invalid("provider derivation is not valid JSON")
        })?;
    if !is_sha256(&declared.payload.root_sha256) {
        return Err(crate::catalog::CatalogError::invalid(
            "provider derivation payload root is not a lowercase SHA-256",
        ));
    }
    let app_root = FsPath::new(&artifact.storage_path)
        .parent()
        .ok_or_else(|| {
            crate::catalog::CatalogError::BundleStorage(
                "canonical artifact storage path has no application directory".into(),
            )
        })?;
    let staging_path = app_root.join(format!(".provider-upload-{}", CanonicalId::new_v7()));
    fs::create_dir(&staging_path).map_err(|error| {
        crate::catalog::CatalogError::BundleStorage(format!(
            "cannot create provider envelope staging directory: {error}"
        ))
    })?;
    let staging = ProviderStaging(staging_path);
    let payload_root = staging.0.join(&declared.payload.root_sha256);
    let record_root = staging.0.join("records");
    fs::create_dir_all(&payload_root)
        .and_then(|_| fs::create_dir_all(&record_root))
        .map_err(|error| {
            crate::catalog::CatalogError::BundleStorage(format!(
                "cannot prepare provider envelope staging tree: {error}"
            ))
        })?;
    write_new(&payload_root.join("bundle.zip"), &bundle)?;
    let record_path = record_root.join(format!("{derivation_sha256}.json"));
    write_new(&record_path, &derivation)?;
    let summary = deis_artifact::provider_derivation::validate(
        &record_path,
        &payload_root,
        Some("deislet"),
        Some(&artifact.standard_version),
        Some(&artifact.artifact_sha256),
    )
    .map_err(|error| crate::catalog::CatalogError::invalid(error.to_string()))?;
    if summary.parent_content_root_sha256 != artifact.content_root_sha256 {
        return Err(crate::catalog::CatalogError::invalid(
            "provider derivation content root differs from its canonical parent",
        ));
    }
    let manifest = String::from_utf8(derivation).map_err(|_| {
        crate::catalog::CatalogError::invalid("provider derivation is not UTF-8 JSON")
    })?;
    Ok(ValidatedEnvelope {
        bundle,
        spec: ProviderDerivationSpec {
            manifest_sha256: summary.manifest_sha256,
            provider: "deislet".into(),
            standard_version: artifact.standard_version.clone(),
            payload_root_sha256: summary.payload_root_sha256,
            manifest,
        },
    })
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn write_new(path: &FsPath, bytes: &[u8]) -> crate::catalog::CatalogResult<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            crate::catalog::CatalogError::BundleStorage(format!(
                "cannot create provider envelope staging file: {error}"
            ))
        })?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| {
            crate::catalog::CatalogError::BundleStorage(format!(
                "cannot write provider envelope staging file: {error}"
            ))
        })
}

struct ProviderStaging(PathBuf);

impl Drop for ProviderStaging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// List an application's versions.
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/versions",
    operation_id = "listVersions",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ListQuery),
    responses(
        (status = 200, description = "One page of versions.", body = ControlListPage<VersionView>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_versions(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<VersionView>>> {
    let app = state.store.get_app(&app_id).await?;
    let collection = "application-versions";
    let binding = format!(
        "{};filter=all;sort=number-desc,version-id-desc-v1",
        id_scope("application", app.canonical_id)
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .store
        .list_versions_page(&app_id, limit, cursor)
        .await?;
    let next_position = page.items.last().map(|version| PagePosition {
        sort_value: version.number,
        identity: version.id.clone(),
        secondary: None,
    });
    let page = CatalogPage {
        items: page.items.iter().map(VersionView::from).collect(),
        total: page.total,
        has_more: page.has_more,
    };
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        move |_| next_position.expect("a page with a successor has a last version"),
    )
    .await?;
    Ok(Json(response))
}

/// Read one version.
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/versions/{version_id}",
    operation_id = "getVersion",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("version_id" = String, Path, description = "The version's id.")),
    responses(
        (status = 200, description = "The version.", body = VersionView),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
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
/// Where a machine downloads one version's bundle from.
///
/// Spelled once because two listeners mount it: the machine channel, which is
/// where it belongs, and - until the fleet has moved - the node router on the
/// human listener. Two spellings of one route is the mistake
/// `scripts/check-doc-path-literals.sh` exists to catch in documents, and it
/// is no better in code.
pub const BUNDLE_ROUTE: &str = "/api/apps/:app_id/versions/:version_id/bundle";

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

#[cfg(test)]
mod lineage_tests {
    use super::*;
    use crate::catalog::CanonicalArtifact;
    use std::io::Write;

    #[test]
    fn an_untrusted_payload_root_is_rejected_before_staging() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("application/canonical");
        let parent = crate::catalog::artifact::tests::source_artifact(&root);
        let artifact = CanonicalArtifact {
            id: CanonicalId::new_v7(),
            app_id: "demo".into(),
            artifact_sha256: parent.artifact_sha256.clone(),
            content_root_sha256: parent.content_root_sha256.clone(),
            standard_version: parent.standard_version.clone(),
            file_count: parent.file_count as i64,
            storage_path: parent.root.to_string_lossy().into_owned(),
            record: serde_json::Value::Null,
            created_at: 1,
        };
        let derivation = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "format": deis_artifact::provider_derivation::FORMAT,
            "provider": "deislet",
            "standardVersion": parent.standard_version,
            "parent": {
                "artifactSha256": parent.artifact_sha256,
                "contentRootSha256": parent.content_root_sha256
            },
            "payload": {
                "algorithm": "sha256-merkle-v1",
                "rootSha256": "../../escape",
                "files": []
            }
        }))
        .unwrap();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::FileOptions::default().unix_permissions(0o644);
        writer.start_file(DERIVATION_ENTRY, options).unwrap();
        writer.write_all(&derivation).unwrap();
        writer.start_file(BUNDLE_ENTRY, options).unwrap();
        writer.write_all(b"bundle").unwrap();
        let envelope = writer.finish().unwrap().into_inner();

        let error = validate_deislet_envelope(&artifact, &envelope).unwrap_err();
        assert!(error.to_string().contains("lowercase SHA-256"), "{error}");
        assert!(!temp.path().join("application/escape").exists());
    }
}
