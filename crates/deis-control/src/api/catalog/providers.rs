//! Provider-neutral management API.
//!
//! Credential values enter only the vault write handlers. They are never
//! returned, formatted with `Debug`, copied into an operation, or inspected
//! by the authorization middleware. Every opaque provider resource lives on
//! the installation-wide administrator surface. An administrator explicitly
//! grants a target to one application environment; deployment credentials can
//! start work only through that durable, revision-pinned grant.
//!
//! Every create here requires an `Idempotency-Key` and every `If-Match`
//! mutation accepts one. A repeated key with the same principal, route and
//! canonical request replays the resource it produced; the same key with a
//! different request is refused with the stable `idempotency-key-reused`
//! code. Provider deployments keep their own durable operation replay.

use axum::{
    Json,
    extract::{Extension, Multipart, Path, Query, State, multipart::Field},
    http::{HeaderMap, HeaderValue, StatusCode, header},
};
use deis_provider_driver::{
    ArtifactVersionRef, CredentialMaterial, HostOperatingSystem, ProviderNeutralPlan, Sha256Digest,
    ValidatedDriverReleaseAuthorization, ValidatedDriverReleaseTransparencyProof,
    ValidatedProviderDriverManifest, ValidatedProviderDriverRelease,
};
use deis_wire::{CanonicalId, ProductOperation};
use serde::Deserialize;
use serde_json::json;
use std::future::Future;
use tokio::io::AsyncWriteExt;
use utoipa::IntoParams;
use utoipa::ToSchema;

use super::{CatalogState, actor_subject};
use crate::api::headers::{optional_header, required_header, single_header};
use crate::api::openapi;
use crate::{
    api::{
        error::{ApiError, ApiResult},
        pagination::{ControlListPage, ListQuery, list_page},
        paths::ApiPaths,
    },
    catalog::{
        CatalogError, IdempotencyDigestKey, IdempotencyOutcome, IdempotencyScope, IdempotencyStore,
        IdempotentRequest, IdempotentResource, OperationRequestContext, PagePosition,
        PendingIdempotencyRecord, ProviderCredentialAuditEvent, ProviderCredentialBinding,
        ProviderCredentialBindingSpec, ProviderCredentialMutationContext,
        ProviderDeploymentCancellation, ProviderDeploymentSpec, ProviderDeploymentWorkflow,
        ProviderDriverReleaseAuditEvent, ProviderDriverReleaseRecord,
        ProviderDriverReleaseRegistry, ProviderInstallation, ProviderInstallationSpec,
        ProviderLifecycleAuditEvent, ProviderLifecycleMutationContext,
        ProviderLifecycleResourceKind, ProviderRollbackSpec, ProviderTarget, ProviderTargetGrant,
        ProviderTargetGrantAuditEvent, ProviderTargetGrantHistoryFilter,
        ProviderTargetGrantMutationContext, ProviderTargetGrantSpec,
        ProviderTargetGrantStatusFilter, ProviderTargetSpec, SecretsBackend,
    },
    identity::Principal,
};

const IDEMPOTENCY_KEY: &str = "idempotency-key";
const REQUEST_ID: &str = "x-request-id";
const CORRELATION_ID: &str = "x-correlation-id";
const IF_MATCH: &str = "if-match";
/// Route ids stored beside every idempotency record. They name the
/// authorization action of the same route so an operator can join the
/// record to the audit line that admitted the request.
const ROUTE_CREDENTIAL_PROVISION: &str = "provider_credential.provision";
const ROUTE_CREDENTIAL_ROTATE: &str = "provider_credential.rotate";
const ROUTE_CREDENTIAL_REVOKE: &str = "provider_credential.revoke";
const ROUTE_INSTALLATION_REGISTER: &str = "provider_installation.register";
const ROUTE_INSTALLATION_DISABLE: &str = "provider_installation.disable";
const ROUTE_INSTALLATION_ENABLE: &str = "provider_installation.enable";
const ROUTE_INSTALLATION_DELETE: &str = "provider_installation.delete";
const ROUTE_TARGET_REGISTER: &str = "provider_target.register";
const ROUTE_TARGET_DISABLE: &str = "provider_target.disable";
const ROUTE_TARGET_ENABLE: &str = "provider_target.enable";
const ROUTE_TARGET_DELETE: &str = "provider_target.delete";
const ROUTE_TARGET_GRANT_CREATE: &str = "provider_target_grant.create";
const ROUTE_TARGET_GRANT_REVOKE: &str = "provider_target_grant.revoke";
const MAX_RELEASE_DOCUMENT_BYTES: usize = 1024 * 1024;
const RELEASE_UPLOAD_PREFIX: &str = ".driver-upload.";
/// The release contract permits a 4 GiB host archive. The remaining 5 MiB
/// covers four bounded signed JSON documents and multipart framing.
pub const MAX_PROVIDER_DRIVER_RELEASE_REQUEST_BYTES: usize =
    4 * 1024 * 1024 * 1024 + 5 * 1024 * 1024;

/// The five parts of a driver release upload, in this order.
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct DriverReleaseUpload {
    /// The release document, canonical JSON
    /// (`contracts/v1/provider-driver-release.schema.json`).
    #[schema(value_type = String)]
    pub release: String,
    /// The driver manifest, canonical JSON
    /// (`contracts/v1/provider-driver.schema.json`).
    #[schema(value_type = String)]
    pub manifest: String,
    /// The release authorization envelope, canonical JSON
    /// (`contracts/v1/provider-driver-release-envelope.schema.json`).
    #[schema(value_type = String)]
    pub authorization: String,
    /// The transparency proof, canonical JSON
    /// (`contracts/v1/provider-driver-transparency-proof.schema.json`).
    #[schema(value_type = String)]
    pub transparency: String,
    /// The driver archive the release names.
    #[schema(value_type = String, format = Binary)]
    pub archive: Vec<u8>,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProvisionCredentialRequest {
    pub backend_id: String,
    pub name: String,
    pub credential: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RotateCredentialRequest {
    pub credential: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisterInstallationRequest {
    pub backend_id: String,
    pub account_reference: String,
    pub region: Option<String>,
    #[schema(value_type = String, format = Uuid)]
    pub credential_binding_id: CanonicalId,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisterTargetRequest {
    pub name: String,
    pub standard_version: String,
    #[schema(value_type = String)]
    pub target_config_digest: Sha256Digest,
    #[schema(value_type = crate::api::openapi::ProviderDriverManifestDocument)]
    pub driver_manifest: serde_json::Value,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BeginProviderDeploymentRequest {
    #[schema(value_type = String, format = Uuid)]
    pub provider_target_id: CanonicalId,
    #[schema(value_type = crate::api::openapi::ArtifactVersionRefDocument)]
    pub artifact: ArtifactVersionRef,
    #[serde(default)]
    #[schema(value_type = crate::api::openapi::ProviderNeutralPlanDocument)]
    pub plan: ProviderNeutralPlan,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantProviderTargetRequest {
    #[schema(value_type = String, format = Uuid)]
    pub provider_target_id: CanonicalId,
    pub note: Option<String>,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct ProviderTargetGrantListQuery {
    /// Page size, `1..=1000`; absent means 100.
    #[param(minimum = 1, maximum = 1000)]
    pub limit: Option<usize>,
    /// The `page.nextCursor` of the previous answer. Opaque, bound to the
    /// collection and the filters it was issued for, and refused with 400
    /// `cursor-invalid` anywhere else.
    pub cursor: Option<String>,
    /// `active` (the default), `revoked` or `all`.
    pub status: Option<String>,
}

impl ProviderTargetGrantListQuery {
    fn page_query(&self) -> ListQuery {
        ListQuery {
            limit: self.limit,
            cursor: self.cursor.clone(),
        }
    }

    fn status_filter(&self) -> ApiResult<ProviderTargetGrantStatusFilter> {
        grant_status_filter(self.status.as_deref().unwrap_or("active"))
    }
}

/// `GET /api/providers/target-grants`: the whole platform's grant history.
/// The status defaults to `all` because this is history, not the live
/// authorization view a single environment lists.
#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct ProviderTargetGrantHistoryQuery {
    /// Page size, `1..=1000`; absent means 100.
    #[param(minimum = 1, maximum = 1000)]
    pub limit: Option<usize>,
    /// The `page.nextCursor` of the previous answer. Opaque, bound to the
    /// collection and the filters it was issued for, and refused with 400
    /// `cursor-invalid` anywhere else.
    pub cursor: Option<String>,
    /// `all` (the default), `active` or `revoked`.
    pub status: Option<String>,
    /// A provider target's id; narrows the history to grants on that target.
    pub target: Option<String>,
}

impl ProviderTargetGrantHistoryQuery {
    fn page_query(&self) -> ListQuery {
        ListQuery {
            limit: self.limit,
            cursor: self.cursor.clone(),
        }
    }

    fn filter(&self) -> ApiResult<ProviderTargetGrantHistoryFilter> {
        Ok(ProviderTargetGrantHistoryFilter {
            status: grant_status_filter(self.status.as_deref().unwrap_or("all"))?,
            provider_target_id: self
                .target
                .as_deref()
                .map(|target| parse_id(target, "provider target"))
                .transpose()?,
        })
    }
}

fn grant_status_filter(status: &str) -> ApiResult<ProviderTargetGrantStatusFilter> {
    match status {
        "active" => Ok(ProviderTargetGrantStatusFilter::Active),
        "revoked" => Ok(ProviderTargetGrantStatusFilter::Revoked),
        "all" => Ok(ProviderTargetGrantStatusFilter::All),
        status => Err(ApiError::BadRequest(format!(
            "provider target grant status must be active, revoked, or all; got {status}"
        ))),
    }
}

/// `DELETE /api/providers/installations/:id?cascade=true`. Without
/// `cascade` an installation that still has targets or active grants is
/// refused with `provider-installation-has-dependents`.
#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct DeleteInstallationQuery {
    /// Also revoke the installation's targets and active grants. Off by
    /// default, in which case an installation with dependents is refused
    /// with `provider-installation-has-dependents`.
    pub cascade: Option<bool>,
}

#[utoipa::path(
    post,
    path = "/api/v1/providers/driver-releases",
    operation_id = "installDriverRelease",
    tag = "product",
    params(("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer.")),
    request_body(content = DriverReleaseUpload, content_type = "multipart/form-data", description = "The release documents and the archive, in this order."),
    responses(
        (status = 201, description = "The release was verified and installed.", body = ProviderDriverReleaseRecord, headers(("Location" = String, description = "The new resource."), ("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 200, description = "The same release was already installed.", body = ProviderDriverReleaseRecord, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 409, response = openapi::Conflict),
        (status = 413, response = openapi::PayloadTooLarge),
        (status = 422, response = openapi::UnprocessableEntity),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn install_driver_release(
    State(state): State<CatalogState>,
    paths: ApiPaths,
    mut multipart: Multipart,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderDriverReleaseRecord>)> {
    let registry = provider_registry(&state)?;
    let release = ValidatedProviderDriverRelease::parse_json(
        &read_document(
            next_named_field(&mut multipart, "release").await?,
            "release",
        )
        .await?,
    )
    .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let manifest = ValidatedProviderDriverManifest::parse_json(
        &read_document(
            next_named_field(&mut multipart, "manifest").await?,
            "manifest",
        )
        .await?,
    )
    .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let authorization = ValidatedDriverReleaseAuthorization::parse_json(
        &read_document(
            next_named_field(&mut multipart, "authorization").await?,
            "authorization",
        )
        .await?,
    )
    .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let transparency = ValidatedDriverReleaseTransparencyProof::parse_json(
        &read_document(
            next_named_field(&mut multipart, "transparency").await?,
            "transparency",
        )
        .await?,
    )
    .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let expected_archive_bytes =
        registry.validate_release_upload(&release, &manifest, &authorization, &transparency)?;

    let archive = next_named_field(&mut multipart, "archive").await?;
    let upload = tempfile::Builder::new()
        .prefix(RELEASE_UPLOAD_PREFIX)
        .tempfile_in(registry.installation_root())
        .map_err(|error| {
            ApiError::InternalError(format!(
                "cannot create provider driver upload staging file: {error}"
            ))
        })?;
    let (file, upload_path) = upload.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let archive_bytes = stream_archive(archive, &mut file, expected_archive_bytes).await?;
    file.flush().await.map_err(|error| {
        ApiError::InternalError(format!(
            "cannot flush provider driver upload staging file: {error}"
        ))
    })?;
    file.sync_all().await.map_err(|error| {
        ApiError::InternalError(format!(
            "cannot synchronize provider driver upload staging file: {error}"
        ))
    })?;
    drop(file);
    if archive_bytes != expected_archive_bytes {
        return Err(ApiError::BadRequest(format!(
            "provider driver archive contains {archive_bytes} bytes; release declares {expected_archive_bytes}"
        )));
    }
    if multipart
        .next_field()
        .await
        .map_err(multipart_error)?
        .is_some()
    {
        return Err(ApiError::BadRequest(
            "provider driver release multipart has fields after archive".to_string(),
        ));
    }

    let release_digest = release
        .canonical_sha256()
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let existed = registry.release(release_digest.clone()).await?.is_some();
    let upload_file_path = upload_path.to_path_buf();
    let installed = registry
        .install_and_register(
            release,
            manifest,
            authorization,
            transparency,
            upload_file_path,
        )
        .await;
    if let Err(error) = upload_path.close() {
        if installed.is_ok() {
            return Err(ApiError::InternalError(format!(
                "cannot remove provider driver upload staging file: {error}"
            )));
        }
        tracing::warn!(%error, "Failed to remove rejected provider driver upload staging file");
    }
    let installed = installed?;
    let status = if existed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let headers = resource_headers(
        &paths.absolute(&format!(
            "/providers/driver-releases/{}",
            installed.release_digest
        )),
        installed.revision,
    )?;
    Ok((status, headers, Json(installed)))
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/driver-releases",
    operation_id = "listDriverReleases",
    tag = "product",
    params(ListQuery),
    responses(
        (status = 200, description = "One page of installed releases.", body = ControlListPage<ProviderDriverReleaseRecord>),
        (status = 400, response = openapi::BadRequest),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_driver_releases(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(
    HeaderMap,
    Json<ControlListPage<ProviderDriverReleaseRecord>>,
)> {
    let collection = "provider-driver-releases";
    let binding = format!(
        "{};filter=all;sort=created-at-desc,release-digest-desc-v1",
        provider_host_scope()?
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = provider_registry(&state)?
        .releases_page(limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |release| PagePosition {
            sort_value: release.created_at.timestamp(),
            identity: release.release_digest.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/driver-releases/{release_digest}",
    operation_id = "getDriverRelease",
    tag = "product",
    params(("release_digest" = String, Path, description = "The release digest, 64 lowercase hexadecimal digits.")),
    responses(
        (status = 200, description = "The release.", body = ProviderDriverReleaseRecord, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_driver_release(
    State(state): State<CatalogState>,
    Path(release_digest): Path<String>,
) -> ApiResult<(HeaderMap, Json<ProviderDriverReleaseRecord>)> {
    let release_digest = parse_digest(&release_digest, "provider driver release")?;
    let release = provider_registry(&state)?
        .release(release_digest.clone())
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "provider driver release {release_digest} not found"
            ))
        })?;
    let headers = resource_headers("", release.revision)?;
    Ok((headers, Json(release)))
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/driver-releases/{release_digest}/audit",
    operation_id = "listDriverReleaseAudit",
    tag = "product",
    params(("release_digest" = String, Path, description = "The release digest, 64 lowercase hexadecimal digits."), ListQuery),
    responses(
        (status = 200, description = "One page of audit events, newest first.", body = ControlListPage<ProviderDriverReleaseAuditEvent>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn driver_release_audit(
    State(state): State<CatalogState>,
    Path(release_digest): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(
    HeaderMap,
    Json<ControlListPage<ProviderDriverReleaseAuditEvent>>,
)> {
    let release_digest = parse_digest(&release_digest, "provider driver release")?;
    let registry = provider_registry(&state)?;
    if registry.release(release_digest.clone()).await?.is_none() {
        return Err(ApiError::NotFound(format!(
            "provider driver release {release_digest} not found"
        )));
    }
    let collection = "provider-driver-release-audit";
    let binding = format!(
        "{};release={};filter=all;sort=observed-at-asc,event-id-asc-v1",
        provider_host_scope()?,
        release_digest
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = registry
        .audit_events_page(release_digest, limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |event| PagePosition {
            sort_value: event.observed_at.timestamp(),
            identity: event.event_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

#[utoipa::path(
    post,
    path = "/api/v1/providers/credentials",
    operation_id = "provisionProviderCredential",
    tag = "product",
    params(("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer.")),
    request_body = ProvisionCredentialRequest,
    responses(
        (status = 201, description = "The credential was sealed and bound.", body = ProviderCredentialBinding, headers(("Location" = String, description = "The new resource."), ("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 409, response = openapi::Conflict),
        (status = 413, response = openapi::PayloadTooLarge),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn provision_credential(
    State(state): State<CatalogState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    paths: ApiPaths,
    Json(request): Json<ProvisionCredentialRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderCredentialBinding>)> {
    let idempotency_key = required_header(&headers, IDEMPOTENCY_KEY)?;
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_CREDENTIAL_PROVISION,
        Some(idempotency_key),
        json!({
            "backendId": request.backend_id,
            "name": request.name,
            "credential": request.credential,
        }),
    )?;
    let material = credential_material(request.credential)?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::CREATED,
        |binding: &ProviderCredentialBinding| {
            paths.absolute(&format!("/providers/credentials/{}", binding.binding_id))
        },
        |record| {
            state.provider_credentials.provision(
                ProviderCredentialBindingSpec {
                    backend_id: request.backend_id,
                    name: request.name,
                },
                material,
                credential_mutation_context(&principal, record),
            )
        },
        |binding_id| state.provider_credentials.binding(binding_id),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/credentials",
    operation_id = "listProviderCredentials",
    tag = "product",
    params(ListQuery),
    responses(
        (status = 200, description = "One page of bindings; never a credential value.", body = ControlListPage<ProviderCredentialBinding>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_credentials(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(HeaderMap, Json<ControlListPage<ProviderCredentialBinding>>)> {
    let collection = "provider-credentials";
    let binding = "scope=default-organization;filter=all;sort=created-at-asc,binding-id-asc-v1";
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, binding)
        .await?;
    let page = state
        .provider_credentials
        .bindings_page(limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        binding,
        limit,
        page,
        |credential| PagePosition {
            sort_value: credential.created_at.timestamp(),
            identity: credential.binding_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/credentials/{binding_id}",
    operation_id = "getProviderCredential",
    tag = "product",
    params(("binding_id" = String, Path, description = "The credential binding's UUIDv7.")),
    responses(
        (status = 200, description = "The binding.", body = ProviderCredentialBinding, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_credential(
    State(state): State<CatalogState>,
    Path(binding_id): Path<String>,
) -> ApiResult<(HeaderMap, Json<ProviderCredentialBinding>)> {
    let binding_id = parse_id(&binding_id, "provider credential binding")?;
    let binding = state
        .provider_credentials
        .binding(binding_id)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "provider credential binding {binding_id} not found"
            ))
        })?;
    let headers = resource_headers("", binding.row_revision)?;
    Ok((headers, Json(binding)))
}

#[utoipa::path(
    post,
    path = "/api/v1/providers/credentials/{binding_id}/rotate",
    operation_id = "rotateProviderCredential",
    tag = "product",
    params(("binding_id" = String, Path, description = "The credential binding's UUIDv7."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer."), ("If-Match" = String, Header, description = "The revision the caller expects, from `ETag`.")),
    request_body = RotateCredentialRequest,
    responses(
        (status = 200, description = "The binding at its new revision.", body = ProviderCredentialBinding, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 412, response = openapi::PreconditionFailed),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 413, response = openapi::PayloadTooLarge),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn rotate_credential(
    State(state): State<CatalogState>,
    Path(binding_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Json(request): Json<RotateCredentialRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderCredentialBinding>)> {
    let binding_id = parse_id(&binding_id, "provider credential binding")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_CREDENTIAL_ROTATE,
        optional_header(&headers, IDEMPOTENCY_KEY)?,
        json!({
            "bindingId": binding_id,
            "ifMatch": expected_revision,
            "credential": request.credential,
        }),
    )?;
    let material = credential_material(request.credential)?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::OK,
        |_| String::new(),
        |record| {
            state.provider_credentials.rotate(
                binding_id,
                expected_revision,
                material,
                credential_mutation_context(&principal, record),
            )
        },
        |binding_id| state.provider_credentials.binding(binding_id),
    )
    .await
}

#[utoipa::path(
    delete,
    path = "/api/v1/providers/credentials/{binding_id}",
    operation_id = "revokeProviderCredential",
    tag = "product",
    params(("binding_id" = String, Path, description = "The credential binding's UUIDv7."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer."), ("If-Match" = String, Header, description = "The revision the caller expects, from `ETag`.")),
    responses(
        (status = 200, description = "The binding, now revoked.", body = ProviderCredentialBinding, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 412, response = openapi::PreconditionFailed),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn revoke_credential(
    State(state): State<CatalogState>,
    Path(binding_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderCredentialBinding>)> {
    let binding_id = parse_id(&binding_id, "provider credential binding")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_CREDENTIAL_REVOKE,
        optional_header(&headers, IDEMPOTENCY_KEY)?,
        json!({"bindingId": binding_id, "ifMatch": expected_revision}),
    )?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::OK,
        |_| String::new(),
        |record| {
            state.provider_credentials.revoke(
                binding_id,
                expected_revision,
                credential_mutation_context(&principal, record),
            )
        },
        |binding_id| state.provider_credentials.binding(binding_id),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/credentials/{binding_id}/audit",
    operation_id = "listProviderCredentialAudit",
    tag = "product",
    params(("binding_id" = String, Path, description = "The credential binding's UUIDv7."), ListQuery),
    responses(
        (status = 200, description = "One page of audit events, newest first.", body = ControlListPage<ProviderCredentialAuditEvent>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn credential_audit(
    State(state): State<CatalogState>,
    Path(binding_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(
    HeaderMap,
    Json<ControlListPage<ProviderCredentialAuditEvent>>,
)> {
    let binding_id = parse_id(&binding_id, "provider credential binding")?;
    if state
        .provider_credentials
        .binding(binding_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(format!(
            "provider credential binding {binding_id} not found"
        )));
    }
    let collection = "provider-credential-audit";
    let binding = format!(
        "scope=provider-credential:{binding_id};filter=all;sort=occurred-at-asc,event-id-asc-v1"
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .provider_credentials
        .audit_events_page(binding_id, limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |event| PagePosition {
            sort_value: event.occurred_at.timestamp(),
            identity: event.event_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

#[utoipa::path(
    post,
    path = "/api/v1/providers/installations",
    operation_id = "registerProviderInstallation",
    tag = "product",
    params(("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer.")),
    request_body = RegisterInstallationRequest,
    responses(
        (status = 201, description = "The installation was registered.", body = ProviderInstallation, headers(("Location" = String, description = "The new resource."), ("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn register_installation(
    State(state): State<CatalogState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    paths: ApiPaths,
    Json(request): Json<RegisterInstallationRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderInstallation>)> {
    let idempotency_key = required_header(&headers, IDEMPOTENCY_KEY)?;
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_INSTALLATION_REGISTER,
        Some(idempotency_key),
        json!({
            "backendId": request.backend_id,
            "accountReference": request.account_reference,
            "region": request.region,
            "credentialBindingId": request.credential_binding_id,
        }),
    )?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::CREATED,
        |installation: &ProviderInstallation| {
            paths.absolute(&format!(
                "/providers/installations/{}",
                installation.installation_id
            ))
        },
        |record| {
            state.store.register_provider_installation(
                ProviderInstallationSpec {
                    backend_id: request.backend_id,
                    account_reference: request.account_reference,
                    region: request.region,
                    credential_binding_id: request.credential_binding_id,
                },
                record,
            )
        },
        |installation_id| state.store.provider_installation(installation_id),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/installations",
    operation_id = "listProviderInstallations",
    tag = "product",
    params(ListQuery),
    responses(
        (status = 200, description = "One page of installations.", body = ControlListPage<ProviderInstallation>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_installations(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(HeaderMap, Json<ControlListPage<ProviderInstallation>>)> {
    let collection = "provider-installations";
    let binding =
        "scope=default-organization;filter=all;sort=created-at-asc,installation-id-asc-v1";
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, binding)
        .await?;
    let page = state
        .store
        .provider_installations_page(limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        binding,
        limit,
        page,
        |installation| PagePosition {
            sort_value: installation.created_at.timestamp(),
            identity: installation.installation_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/installations/{installation_id}",
    operation_id = "getProviderInstallation",
    tag = "product",
    params(("installation_id" = String, Path, description = "The installation's UUIDv7.")),
    responses(
        (status = 200, description = "The installation.", body = ProviderInstallation, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_installation(
    State(state): State<CatalogState>,
    Path(installation_id): Path<String>,
) -> ApiResult<(HeaderMap, Json<ProviderInstallation>)> {
    let installation_id = parse_id(&installation_id, "provider installation")?;
    let installation = state
        .store
        .provider_installation(installation_id)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!("provider installation {installation_id} not found"))
        })?;
    let headers = resource_headers("", installation.revision)?;
    Ok((headers, Json(installation)))
}

#[utoipa::path(
    post,
    path = "/api/v1/providers/installations/{installation_id}/targets",
    operation_id = "registerProviderTarget",
    tag = "product",
    params(("installation_id" = String, Path, description = "The installation's UUIDv7."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer.")),
    request_body = RegisterTargetRequest,
    responses(
        (status = 201, description = "The target was registered against an installed driver release.", body = ProviderTarget, headers(("Location" = String, description = "The new resource."), ("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 422, response = openapi::UnprocessableEntity),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn register_target(
    State(state): State<CatalogState>,
    Path(installation_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    paths: ApiPaths,
    Json(request): Json<RegisterTargetRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderTarget>)> {
    let idempotency_key = required_header(&headers, IDEMPOTENCY_KEY)?;
    let installation_id = parse_id(&installation_id, "provider installation")?;
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_TARGET_REGISTER,
        Some(idempotency_key),
        json!({
            "installationId": installation_id,
            "name": request.name,
            "standardVersion": request.standard_version,
            "targetConfigDigest": request.target_config_digest,
            "driverManifest": request.driver_manifest,
        }),
    )?;
    let manifest_bytes = serde_json::to_vec(&request.driver_manifest).map_err(|error| {
        ApiError::BadRequest(format!(
            "provider driver manifest is not valid JSON: {error}"
        ))
    })?;
    let driver_manifest =
        ValidatedProviderDriverManifest::parse_json(&manifest_bytes).map_err(|error| {
            ApiError::BadRequest(format!("invalid provider driver manifest: {error}"))
        })?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::CREATED,
        |target: &ProviderTarget| {
            paths.absolute(&format!("/providers/targets/{}", target.target_id))
        },
        |record| {
            state.store.register_provider_target(
                ProviderTargetSpec {
                    installation_id,
                    name: request.name,
                    standard_version: request.standard_version,
                    target_config_digest: request.target_config_digest,
                    driver_manifest,
                },
                record,
            )
        },
        |target_id| state.store.provider_target(target_id),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/installations/{installation_id}/targets",
    operation_id = "listProviderInstallationTargets",
    tag = "product",
    params(("installation_id" = String, Path, description = "The installation's UUIDv7."), ListQuery),
    responses(
        (status = 200, description = "One page of the installation's targets.", body = ControlListPage<ProviderTarget>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_installation_targets(
    State(state): State<CatalogState>,
    Path(installation_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(HeaderMap, Json<ControlListPage<ProviderTarget>>)> {
    let installation_id = parse_id(&installation_id, "provider installation")?;
    let collection = "provider-targets";
    let binding = format!(
        "scope=provider-installation:{installation_id};filter=all;sort=created-at-asc,target-id-asc-v1"
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .store
        .provider_targets_page(Some(installation_id), limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |target| PagePosition {
            sort_value: target.created_at.timestamp(),
            identity: target.target_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/targets",
    operation_id = "listProviderTargets",
    tag = "product",
    params(ListQuery),
    responses(
        (status = 200, description = "One page of targets.", body = ControlListPage<ProviderTarget>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_targets(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(HeaderMap, Json<ControlListPage<ProviderTarget>>)> {
    let collection = "provider-targets";
    let binding = "scope=default-organization;filter=all;sort=created-at-asc,target-id-asc-v1";
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, binding)
        .await?;
    let page = state
        .store
        .provider_targets_page(None, limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        binding,
        limit,
        page,
        |target| PagePosition {
            sort_value: target.created_at.timestamp(),
            identity: target.target_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/targets/{target_id}",
    operation_id = "getProviderTarget",
    tag = "product",
    params(("target_id" = String, Path, description = "The target's UUIDv7.")),
    responses(
        (status = 200, description = "The target.", body = ProviderTarget, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_target(
    State(state): State<CatalogState>,
    Path(target_id): Path<String>,
) -> ApiResult<(HeaderMap, Json<ProviderTarget>)> {
    let target_id = parse_id(&target_id, "provider target")?;
    let target = state
        .store
        .provider_target(target_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("provider target {target_id} not found")))?;
    let headers = resource_headers("", target.revision)?;
    Ok((headers, Json(target)))
}

#[utoipa::path(
    post,
    path = "/api/v1/providers/installations/{installation_id}/disable",
    operation_id = "disableProviderInstallation",
    tag = "product",
    params(("installation_id" = String, Path, description = "The installation's UUIDv7."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer."), ("If-Match" = String, Header, description = "The revision the caller expects, from `ETag`.")),
    responses(
        (status = 200, description = "The installation, now disabled.", body = ProviderInstallation, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 412, response = openapi::PreconditionFailed),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn disable_installation(
    State(state): State<CatalogState>,
    Path(installation_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderInstallation>)> {
    let installation_id = parse_id(&installation_id, "provider installation")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_INSTALLATION_DISABLE,
        optional_header(&headers, IDEMPOTENCY_KEY)?,
        json!({"installationId": installation_id, "ifMatch": expected_revision}),
    )?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::OK,
        |_| String::new(),
        |record| {
            state.store.disable_provider_installation(
                installation_id,
                expected_revision,
                lifecycle_mutation_context(&principal, record),
            )
        },
        |installation_id| state.store.provider_installation(installation_id),
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/v1/providers/installations/{installation_id}/enable",
    operation_id = "enableProviderInstallation",
    tag = "product",
    params(("installation_id" = String, Path, description = "The installation's UUIDv7."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer."), ("If-Match" = String, Header, description = "The revision the caller expects, from `ETag`.")),
    responses(
        (status = 200, description = "The installation, now enabled.", body = ProviderInstallation, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 412, response = openapi::PreconditionFailed),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn enable_installation(
    State(state): State<CatalogState>,
    Path(installation_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderInstallation>)> {
    let installation_id = parse_id(&installation_id, "provider installation")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_INSTALLATION_ENABLE,
        optional_header(&headers, IDEMPOTENCY_KEY)?,
        json!({"installationId": installation_id, "ifMatch": expected_revision}),
    )?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::OK,
        |_| String::new(),
        |record| {
            state.store.enable_provider_installation(
                installation_id,
                expected_revision,
                lifecycle_mutation_context(&principal, record),
            )
        },
        |installation_id| state.store.provider_installation(installation_id),
    )
    .await
}

/// Soft delete. The response is the installation with `deletedAt` set and
/// status `revoked`; its targets, grants, workflows and audit stay readable.
#[utoipa::path(
    delete,
    path = "/api/v1/providers/installations/{installation_id}",
    operation_id = "deleteProviderInstallation",
    tag = "product",
    params(("installation_id" = String, Path, description = "The installation's UUIDv7."), DeleteInstallationQuery, ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer."), ("If-Match" = String, Header, description = "The revision the caller expects, from `ETag`.")),
    responses(
        (status = 200, description = "The installation, now deleted; with `cascade=true` its targets and their grants too.", body = ProviderInstallation, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 412, response = openapi::PreconditionFailed),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn delete_installation(
    State(state): State<CatalogState>,
    Path(installation_id): Path<String>,
    Query(query): Query<DeleteInstallationQuery>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderInstallation>)> {
    let installation_id = parse_id(&installation_id, "provider installation")?;
    let expected_revision = required_if_match(&headers)?;
    let cascade = query.cascade.unwrap_or(false);
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_INSTALLATION_DELETE,
        optional_header(&headers, IDEMPOTENCY_KEY)?,
        json!({
            "installationId": installation_id,
            "ifMatch": expected_revision,
            "cascade": cascade,
        }),
    )?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::OK,
        |_| String::new(),
        |record| {
            state.store.delete_provider_installation(
                installation_id,
                expected_revision,
                cascade,
                lifecycle_mutation_context(&principal, record),
            )
        },
        |installation_id| state.store.provider_installation(installation_id),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/installations/{installation_id}/audit",
    operation_id = "listProviderInstallationAudit",
    tag = "product",
    params(("installation_id" = String, Path, description = "The installation's UUIDv7."), ListQuery),
    responses(
        (status = 200, description = "One page of lifecycle events, newest first.", body = ControlListPage<ProviderLifecycleAuditEvent>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn installation_audit(
    State(state): State<CatalogState>,
    Path(installation_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(
    HeaderMap,
    Json<ControlListPage<ProviderLifecycleAuditEvent>>,
)> {
    let installation_id = parse_id(&installation_id, "provider installation")?;
    lifecycle_audit_page(
        &state,
        ProviderLifecycleResourceKind::Installation,
        installation_id,
        query,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/v1/providers/targets/{target_id}/disable",
    operation_id = "disableProviderTarget",
    tag = "product",
    params(("target_id" = String, Path, description = "The target's UUIDv7."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer."), ("If-Match" = String, Header, description = "The revision the caller expects, from `ETag`.")),
    responses(
        (status = 200, description = "The target, now disabled.", body = ProviderTarget, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 412, response = openapi::PreconditionFailed),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn disable_target(
    State(state): State<CatalogState>,
    Path(target_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderTarget>)> {
    let target_id = parse_id(&target_id, "provider target")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_TARGET_DISABLE,
        optional_header(&headers, IDEMPOTENCY_KEY)?,
        json!({"targetId": target_id, "ifMatch": expected_revision}),
    )?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::OK,
        |_| String::new(),
        |record| {
            state.store.disable_provider_target(
                target_id,
                expected_revision,
                lifecycle_mutation_context(&principal, record),
            )
        },
        |target_id| state.store.provider_target(target_id),
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/v1/providers/targets/{target_id}/enable",
    operation_id = "enableProviderTarget",
    tag = "product",
    params(("target_id" = String, Path, description = "The target's UUIDv7."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer."), ("If-Match" = String, Header, description = "The revision the caller expects, from `ETag`.")),
    responses(
        (status = 200, description = "The target, now enabled.", body = ProviderTarget, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 412, response = openapi::PreconditionFailed),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn enable_target(
    State(state): State<CatalogState>,
    Path(target_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderTarget>)> {
    let target_id = parse_id(&target_id, "provider target")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_TARGET_ENABLE,
        optional_header(&headers, IDEMPOTENCY_KEY)?,
        json!({"targetId": target_id, "ifMatch": expected_revision}),
    )?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::OK,
        |_| String::new(),
        |record| {
            state.store.enable_provider_target(
                target_id,
                expected_revision,
                lifecycle_mutation_context(&principal, record),
            )
        },
        |target_id| state.store.provider_target(target_id),
    )
    .await
}

/// Soft delete. Active grants on the target are revoked in the same
/// transaction; a non-terminal provider deployment workflow on it makes the
/// request a `provider-target-in-use` conflict that lists the operations.
#[utoipa::path(
    delete,
    path = "/api/v1/providers/targets/{target_id}",
    operation_id = "deleteProviderTarget",
    tag = "product",
    params(("target_id" = String, Path, description = "The target's UUIDv7."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer."), ("If-Match" = String, Header, description = "The revision the caller expects, from `ETag`.")),
    responses(
        (status = 200, description = "The target, now deleted, and its grants revoked.", body = ProviderTarget, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 412, response = openapi::PreconditionFailed),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn delete_target(
    State(state): State<CatalogState>,
    Path(target_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderTarget>)> {
    let target_id = parse_id(&target_id, "provider target")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_TARGET_DELETE,
        optional_header(&headers, IDEMPOTENCY_KEY)?,
        json!({"targetId": target_id, "ifMatch": expected_revision}),
    )?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::OK,
        |_| String::new(),
        |record| {
            state.store.delete_provider_target(
                target_id,
                expected_revision,
                lifecycle_mutation_context(&principal, record),
            )
        },
        |target_id| state.store.provider_target(target_id),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/targets/{target_id}/audit",
    operation_id = "listProviderTargetAudit",
    tag = "product",
    params(("target_id" = String, Path, description = "The target's UUIDv7."), ListQuery),
    responses(
        (status = 200, description = "One page of lifecycle events, newest first.", body = ControlListPage<ProviderLifecycleAuditEvent>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn target_audit(
    State(state): State<CatalogState>,
    Path(target_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(
    HeaderMap,
    Json<ControlListPage<ProviderLifecycleAuditEvent>>,
)> {
    let target_id = parse_id(&target_id, "provider target")?;
    lifecycle_audit_page(
        &state,
        ProviderLifecycleResourceKind::Target,
        target_id,
        query,
    )
    .await
}

async fn lifecycle_audit_page(
    state: &CatalogState,
    kind: ProviderLifecycleResourceKind,
    resource_id: CanonicalId,
    query: ListQuery,
) -> ApiResult<(
    HeaderMap,
    Json<ControlListPage<ProviderLifecycleAuditEvent>>,
)> {
    let collection = "provider-lifecycle-audit";
    let binding = format!(
        "scope={}:{resource_id};filter=all;sort=resource-revision-asc,event-id-asc-v1",
        kind.as_str()
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .store
        .provider_lifecycle_audit_events_page(kind, resource_id, limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |event| PagePosition {
            sort_value: i64::try_from(event.resource_revision)
                .expect("stored provider lifecycle revision fits SQLite INTEGER"),
            identity: event.event_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

/// Every grant on the platform, newest first, including grants whose
/// application no longer exists. Administrator-only, like every other
/// installation-wide provider resource.
#[utoipa::path(
    get,
    path = "/api/v1/providers/target-grants",
    operation_id = "listProviderTargetGrantHistory",
    tag = "product",
    params(ProviderTargetGrantHistoryQuery),
    responses(
        (status = 200, description = "One page of grants across every application.", body = ControlListPage<ProviderTargetGrant>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_provider_target_grant_history(
    State(state): State<CatalogState>,
    Query(query): Query<ProviderTargetGrantHistoryQuery>,
) -> ApiResult<(HeaderMap, Json<ControlListPage<ProviderTargetGrant>>)> {
    let filter = query.filter()?;
    let collection = "provider-target-grant-history";
    let binding = format!(
        "scope=default-organization;filter=status:{},target:{};sort=created-at-desc,grant-id-desc-v1",
        filter.status.as_str(),
        filter
            .provider_target_id
            .map_or_else(|| "any".to_owned(), |id| id.to_string())
    );
    let page_query = query.page_query();
    let limit = page_query.bounded_limit()?;
    let cursor = page_query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .store
        .provider_target_grant_history_page(filter, limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |grant| PagePosition {
            sort_value: grant.created_at.timestamp(),
            identity: grant.grant_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

#[utoipa::path(
    get,
    path = "/api/v1/providers/target-grants/{grant_id}",
    operation_id = "getProviderTargetGrantHistory",
    tag = "product",
    params(("grant_id" = String, Path, description = "The grant's UUIDv7.")),
    responses(
        (status = 200, description = "The grant.", body = ProviderTargetGrant, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_provider_target_grant_history(
    State(state): State<CatalogState>,
    Path(grant_id): Path<String>,
) -> ApiResult<(HeaderMap, Json<ProviderTargetGrant>)> {
    let grant_id = parse_id(&grant_id, "provider target grant")?;
    let grant = state
        .store
        .provider_target_grant(grant_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("provider target grant {grant_id} not found")))?;
    let headers = resource_headers("", grant.revision)?;
    Ok((headers, Json(grant)))
}

/// The per-grant audit envelope without an application scope, so the trail
/// of a grant remains readable after its application was retired.
#[utoipa::path(
    get,
    path = "/api/v1/providers/target-grants/{grant_id}/audit",
    operation_id = "listProviderTargetGrantHistoryAudit",
    tag = "product",
    params(("grant_id" = String, Path, description = "The grant's UUIDv7."), ListQuery),
    responses(
        (status = 200, description = "One page of grant events, newest first.", body = ControlListPage<ProviderTargetGrantAuditEvent>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn provider_target_grant_history_audit(
    State(state): State<CatalogState>,
    Path(grant_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(
    HeaderMap,
    Json<ControlListPage<ProviderTargetGrantAuditEvent>>,
)> {
    let grant_id = parse_id(&grant_id, "provider target grant")?;
    if state.store.provider_target_grant(grant_id).await?.is_none() {
        return Err(ApiError::NotFound(format!(
            "provider target grant {grant_id} not found"
        )));
    }
    let collection = "provider-target-grant-audit";
    let binding = format!(
        "scope=provider-target-grant:{grant_id};filter=all;sort=grant-revision-asc,event-id-asc-v1"
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .store
        .provider_target_grant_audit_events_page(grant_id, limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |event| PagePosition {
            sort_value: i64::try_from(event.grant_revision)
                .expect("stored provider target grant revision fits SQLite INTEGER"),
            identity: event.event_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_name}/environments/{environment_name}/provider-target-grants",
    operation_id = "grantProviderTarget",
    tag = "product",
    params(("app_name" = String, Path, description = "The application's id."), ("environment_name" = String, Path, description = "The environment's name."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer.")),
    request_body = GrantProviderTargetRequest,
    responses(
        (status = 201, description = "The grant was recorded.", body = ProviderTargetGrant, headers(("Location" = String, description = "The new resource."), ("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn grant_provider_target(
    State(state): State<CatalogState>,
    Path((app_name, environment_name)): Path<(String, String)>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    paths: ApiPaths,
    Json(request): Json<GrantProviderTargetRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderTargetGrant>)> {
    let idempotency_key = required_header(&headers, IDEMPOTENCY_KEY)?;
    let app = state.store.get_app(&app_name).await?;
    let environment = state
        .store
        .get_environment(&app_name, &environment_name)
        .await?;
    // Canonical ids, not names: a name deleted and recreated must not let an
    // old key replay a grant onto the new application.
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_TARGET_GRANT_CREATE,
        Some(idempotency_key),
        json!({
            "applicationId": app.canonical_id,
            "environmentId": environment.canonical_id,
            "providerTargetId": request.provider_target_id,
            "note": request.note,
        }),
    )?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::CREATED,
        |grant: &ProviderTargetGrant| {
            paths.absolute(&format!(
                "/apps/{app_name}/environments/{environment_name}/provider-target-grants/{}",
                grant.grant_id
            ))
        },
        |record| {
            state.store.grant_provider_target(
                ProviderTargetGrantSpec {
                    application_id: app.canonical_id,
                    environment_id: environment.canonical_id,
                    provider_target_id: request.provider_target_id,
                    note: request.note,
                },
                provider_target_grant_mutation_context(&principal, record),
            )
        },
        |grant_id| state.store.provider_target_grant(grant_id),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_name}/environments/{environment_name}/provider-target-grants",
    operation_id = "listProviderTargetGrants",
    tag = "product",
    params(("app_name" = String, Path, description = "The application's id."), ("environment_name" = String, Path, description = "The environment's name."), ProviderTargetGrantListQuery),
    responses(
        (status = 200, description = "One page of the environment's grants.", body = ControlListPage<ProviderTargetGrant>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_provider_target_grants(
    State(state): State<CatalogState>,
    Path((app_name, environment_name)): Path<(String, String)>,
    Query(query): Query<ProviderTargetGrantListQuery>,
) -> ApiResult<(HeaderMap, Json<ControlListPage<ProviderTargetGrant>>)> {
    let app = state.store.get_app(&app_name).await?;
    let environment = state
        .store
        .get_environment(&app_name, &environment_name)
        .await?;
    let status = query.status_filter()?;
    let status_name = status.as_str();
    let collection = "provider-target-grants";
    let binding = format!(
        "scope=application:{}/environment:{};filter=status:{status_name};sort=created-at-desc,grant-id-desc-v1",
        app.canonical_id, environment.canonical_id
    );
    let page_query = query.page_query();
    let limit = page_query.bounded_limit()?;
    let cursor = page_query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .store
        .provider_target_grants_for_environment_page(
            app.canonical_id,
            environment.canonical_id,
            status,
            limit,
            cursor,
        )
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |grant| PagePosition {
            sort_value: grant.created_at.timestamp(),
            identity: grant.grant_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_name}/environments/{environment_name}/provider-target-grants/{grant_id}",
    operation_id = "getProviderTargetGrant",
    tag = "product",
    params(("app_name" = String, Path, description = "The application's id."), ("environment_name" = String, Path, description = "The environment's name."), ("grant_id" = String, Path, description = "The grant's UUIDv7.")),
    responses(
        (status = 200, description = "The grant.", body = ProviderTargetGrant, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_provider_target_grant(
    State(state): State<CatalogState>,
    Path((app_name, environment_name, grant_id)): Path<(String, String, String)>,
) -> ApiResult<(HeaderMap, Json<ProviderTargetGrant>)> {
    let grant_id = parse_id(&grant_id, "provider target grant")?;
    let grant =
        provider_target_grant_in_scope(&state, &app_name, &environment_name, grant_id).await?;
    let headers = resource_headers("", grant.revision)?;
    Ok((headers, Json(grant)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/apps/{app_name}/environments/{environment_name}/provider-target-grants/{grant_id}",
    operation_id = "revokeProviderTargetGrant",
    tag = "product",
    params(("app_name" = String, Path, description = "The application's id."), ("environment_name" = String, Path, description = "The environment's name."), ("grant_id" = String, Path, description = "The grant's UUIDv7."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer."), ("If-Match" = String, Header, description = "The revision the caller expects, from `ETag`.")),
    responses(
        (status = 200, description = "The grant, now revoked.", body = ProviderTargetGrant, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 412, response = openapi::PreconditionFailed),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn revoke_provider_target_grant(
    State(state): State<CatalogState>,
    Path((app_name, environment_name, grant_id)): Path<(String, String, String)>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderTargetGrant>)> {
    let grant_id = parse_id(&grant_id, "provider target grant")?;
    provider_target_grant_in_scope(&state, &app_name, &environment_name, grant_id).await?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
        &state.secrets,
        ROUTE_TARGET_GRANT_REVOKE,
        optional_header(&headers, IDEMPOTENCY_KEY)?,
        json!({"grantId": grant_id, "ifMatch": expected_revision}),
    )?;
    idempotent_mutation(
        &state,
        &principal,
        replay,
        StatusCode::OK,
        |_| String::new(),
        |record| {
            state.store.revoke_provider_target_grant(
                grant_id,
                expected_revision,
                provider_target_grant_mutation_context(&principal, record),
            )
        },
        |grant_id| state.store.provider_target_grant(grant_id),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_name}/environments/{environment_name}/provider-target-grants/{grant_id}/audit",
    operation_id = "listProviderTargetGrantAudit",
    tag = "product",
    params(("app_name" = String, Path, description = "The application's id."), ("environment_name" = String, Path, description = "The environment's name."), ("grant_id" = String, Path, description = "The grant's UUIDv7."), ListQuery),
    responses(
        (status = 200, description = "One page of grant events, newest first.", body = ControlListPage<ProviderTargetGrantAuditEvent>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn provider_target_grant_audit(
    State(state): State<CatalogState>,
    Path((app_name, environment_name, grant_id)): Path<(String, String, String)>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(
    HeaderMap,
    Json<ControlListPage<ProviderTargetGrantAuditEvent>>,
)> {
    let grant_id = parse_id(&grant_id, "provider target grant")?;
    let grant =
        provider_target_grant_in_scope(&state, &app_name, &environment_name, grant_id).await?;
    let collection = "provider-target-grant-audit";
    let binding = format!(
        "scope=application:{}/environment:{}/grant:{};filter=all;sort=grant-revision-asc,event-id-asc-v1",
        grant.application_id, grant.environment_id, grant.grant_id
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .store
        .provider_target_grant_audit_events_page(grant_id, limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |event| PagePosition {
            sort_value: i64::try_from(event.grant_revision)
                .expect("stored provider target grant revision fits SQLite INTEGER"),
            identity: event.event_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

async fn provider_target_grant_in_scope(
    state: &CatalogState,
    app_name: &str,
    environment_name: &str,
    grant_id: CanonicalId,
) -> ApiResult<ProviderTargetGrant> {
    let app = state.store.get_app(app_name).await?;
    let environment = state
        .store
        .get_environment(app_name, environment_name)
        .await?;
    let grant = state
        .store
        .provider_target_grant(grant_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("provider target grant {grant_id} not found")))?;
    if grant.application_id != app.canonical_id || grant.environment_id != environment.canonical_id
    {
        return Err(ApiError::NotFound(format!(
            "provider target grant {grant_id} not found for {app_name}/{environment_name}"
        )));
    }
    Ok(grant)
}

#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_name}/environments/{environment_name}/provider-deployments",
    operation_id = "beginProviderDeployment",
    tag = "product",
    params(("app_name" = String, Path, description = "The application's id."), ("environment_name" = String, Path, description = "The environment's name."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same answer.")),
    request_body = BeginProviderDeploymentRequest,
    responses(
        (status = 202, description = "The deployment was recorded and will be driven; poll `Location`.", body = openapi::ProductOperationDocument, headers(("Location" = String, description = "Where to poll the operation."))),
        (status = 400, response = openapi::BadRequest),
        (status = 403, response = openapi::Forbidden),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn begin_provider_deployment(
    State(state): State<CatalogState>,
    Path((app_name, environment_name)): Path<(String, String)>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    paths: ApiPaths,
    Json(request): Json<BeginProviderDeploymentRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProductOperation>)> {
    let app = state.store.get_app(&app_name).await?;
    let environment = state
        .store
        .get_environment(&app_name, &environment_name)
        .await?;
    let context = operation_context(
        &headers,
        &principal,
        &format!("/api/apps/{app_name}/environments/{environment_name}/provider-deployments"),
    )?;
    let operation = state
        .store
        .begin_provider_deployment(
            ProviderDeploymentSpec {
                application_id: app.canonical_id,
                environment_id: environment.canonical_id,
                provider_target_id: request.provider_target_id,
                artifact: request.artifact,
                plan: request.plan,
            },
            context,
        )
        .await?;
    let headers = operation_headers(paths, &operation, &app_name, &environment_name)?;
    Ok((StatusCode::ACCEPTED, headers, Json(operation)))
}

#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_name}/environments/{environment_name}/provider-deployments",
    operation_id = "listProviderDeployments",
    tag = "product",
    params(("app_name" = String, Path, description = "The application's id."), ("environment_name" = String, Path, description = "The environment's name."), ListQuery),
    responses(
        (status = 200, description = "One page of provider deployments, newest first.", body = ControlListPage<ProviderDeploymentWorkflow>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_provider_deployments(
    State(state): State<CatalogState>,
    Path((app_name, environment_name)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(HeaderMap, Json<ControlListPage<ProviderDeploymentWorkflow>>)> {
    let app = state.store.get_app(&app_name).await?;
    let environment = state
        .store
        .get_environment(&app_name, &environment_name)
        .await?;
    let collection = "provider-deployments";
    let binding = format!(
        "scope=application:{}/environment:{};filter=all;sort=created-at-desc,operation-id-desc-v1",
        app.canonical_id, environment.canonical_id
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .store
        .provider_deployments_for_environment_page(
            app.canonical_id,
            environment.canonical_id,
            limit,
            cursor,
        )
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |workflow| PagePosition {
            sort_value: workflow.operation.created_at.timestamp(),
            identity: workflow.operation.operation_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok((no_store_headers(), Json(response)))
}

#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_name}/environments/{environment_name}/provider-deployments/{operation_id}",
    operation_id = "getProviderDeployment",
    tag = "product",
    params(("app_name" = String, Path, description = "The application's id."), ("environment_name" = String, Path, description = "The environment's name."), ("operation_id" = String, Path, description = "The deployment operation's UUIDv7.")),
    responses(
        (status = 200, description = "The workflow and its operation.", body = ProviderDeploymentWorkflow, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_provider_deployment(
    State(state): State<CatalogState>,
    Path((app_name, environment_name, operation_id)): Path<(String, String, String)>,
    paths: ApiPaths,
) -> ApiResult<(HeaderMap, Json<ProviderDeploymentWorkflow>)> {
    let app = state.store.get_app(&app_name).await?;
    let environment = state
        .store
        .get_environment(&app_name, &environment_name)
        .await?;
    let operation_id = parse_id(&operation_id, "provider deployment operation")?;
    let workflow = state
        .store
        .provider_deployment_workflow(operation_id)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "provider deployment operation {operation_id} not found"
            ))
        })?;
    if workflow.operation.scope.application_id != Some(app.canonical_id)
        || workflow.operation.scope.environment_id != Some(environment.canonical_id)
    {
        return Err(ApiError::NotFound(format!(
            "provider deployment operation {operation_id} not found for {app_name}/{environment_name}"
        )));
    }
    let headers = operation_headers(paths, &workflow.operation, &app_name, &environment_name)?;
    Ok((headers, Json(workflow)))
}

#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_name}/environments/{environment_name}/provider-deployments/{operation_id}/cancel",
    operation_id = "cancelProviderDeployment",
    tag = "product",
    params(("app_name" = String, Path, description = "The application's id."), ("environment_name" = String, Path, description = "The environment's name."), ("operation_id" = String, Path, description = "The deployment or rollback operation's UUIDv7."), ("Idempotency-Key" = Option<String>, Header, description = "Optional. A replay of the same key by the same principal returns the same operation."), ("If-Match" = String, Header, description = "The operation revision the caller expects.")),
    responses(
        (status = 202, description = "Cancellation was requested; poll `Location`. The operation is `cancelling` until in-flight provider work settles and reversible side effects are cleaned up, then `cancelled` (or `failed` with the cleanup diagnostic). Steps that already activated are not undone; that is a rollback.", body = openapi::ProductOperationDocument, headers(("Location" = String, description = "Where to poll the operation."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 412, response = openapi::PreconditionFailed),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn cancel_provider_deployment(
    State(state): State<CatalogState>,
    Path((app_name, environment_name, operation_id)): Path<(String, String, String)>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    paths: ApiPaths,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProductOperation>)> {
    let app = state.store.get_app(&app_name).await?;
    let environment = state
        .store
        .get_environment(&app_name, &environment_name)
        .await?;
    let operation_id = parse_id(&operation_id, "provider deployment operation")?;
    let expected_revision = required_if_match(&headers)?;
    let route = format!(
        "/api/apps/{app_name}/environments/{environment_name}/provider-deployments/{operation_id}/cancel"
    );
    let replay = optional_header(&headers, IDEMPOTENCY_KEY)?
        .map(|key| keyed_operation_context(&headers, &principal, &route, key))
        .transpose()?;
    let operation = state
        .store
        .request_provider_deployment_cancellation(ProviderDeploymentCancellation {
            application_id: app.canonical_id,
            environment_id: environment.canonical_id,
            operation_id,
            expected_revision,
            actor_subject: actor_subject(&principal),
            replay,
        })
        .await?;
    let headers = operation_headers(paths, &operation, &app_name, &environment_name)?;
    Ok((StatusCode::ACCEPTED, headers, Json(operation)))
}

#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_name}/environments/{environment_name}/provider-deployments/{operation_id}/rollback",
    operation_id = "rollbackProviderDeployment",
    tag = "product",
    params(("app_name" = String, Path, description = "The application's id."), ("environment_name" = String, Path, description = "The environment's name."), ("operation_id" = String, Path, description = "The succeeded deployment operation to roll back to."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same operation.")),
    responses(
        (status = 202, description = "A new `rollback` operation was recorded and will be driven; poll `Location`. Its workflow names the deployment in `rolledBackFromOperationId`, runs the driver's `rollback` against that deployment's exact desired state and then `observe`s the result.", body = openapi::ProductOperationDocument, headers(("Location" = String, description = "Where to poll the new operation."))),
        (status = 400, response = openapi::BadRequest),
        (status = 403, response = openapi::Forbidden),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
    )
)]
pub async fn rollback_provider_deployment(
    State(state): State<CatalogState>,
    Path((app_name, environment_name, operation_id)): Path<(String, String, String)>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    paths: ApiPaths,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProductOperation>)> {
    let app = state.store.get_app(&app_name).await?;
    let environment = state
        .store
        .get_environment(&app_name, &environment_name)
        .await?;
    let operation_id = parse_id(&operation_id, "provider deployment operation")?;
    let context = operation_context(
        &headers,
        &principal,
        &format!(
            "/api/apps/{app_name}/environments/{environment_name}/provider-deployments/{operation_id}/rollback"
        ),
    )?;
    let operation = state
        .store
        .begin_provider_rollback(
            ProviderRollbackSpec {
                application_id: app.canonical_id,
                environment_id: environment.canonical_id,
                rolled_back_from_operation_id: operation_id,
            },
            context,
        )
        .await?;
    let headers = operation_headers(paths, &operation, &app_name, &environment_name)?;
    Ok((StatusCode::ACCEPTED, headers, Json(operation)))
}

fn provider_registry(state: &CatalogState) -> ApiResult<ProviderDriverReleaseRegistry> {
    state
        .provider_driver_dispatcher
        .as_ref()
        .map(|dispatcher| dispatcher.registry().clone())
        .ok_or_else(|| {
            ApiError::NotConfigured(
                "provider driver release management is disabled; configure a verified trust root and installation directory"
                    .to_string(),
            )
        })
}

fn provider_host_scope() -> ApiResult<String> {
    let host = HostOperatingSystem::current().ok_or_else(|| {
        ApiError::NotConfigured(
            "provider driver release management is unsupported on this Control host".to_string(),
        )
    })?;
    Ok(format!("scope=host:{}", host.as_str()))
}

async fn next_named_field<'a>(
    multipart: &'a mut Multipart,
    expected: &str,
) -> ApiResult<Field<'a>> {
    let field = multipart
        .next_field()
        .await
        .map_err(multipart_error)?
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "provider driver release multipart is missing the {expected} field"
            ))
        })?;
    if field.name() != Some(expected) {
        return Err(ApiError::BadRequest(format!(
            "provider driver release multipart fields must be release, manifest, authorization, transparency, archive in that order; expected {expected}"
        )));
    }
    Ok(field)
}

async fn read_document(mut field: Field<'_>, label: &str) -> ApiResult<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RELEASE_DOCUMENT_BYTES {
            return Err(ApiError::BadRequest(format!(
                "provider driver {label} document exceeds {MAX_RELEASE_DOCUMENT_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "provider driver {label} document is empty"
        )));
    }
    Ok(bytes)
}

async fn stream_archive(
    mut field: Field<'_>,
    file: &mut tokio::fs::File,
    expected_bytes: u64,
) -> ApiResult<u64> {
    let mut written = 0_u64;
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        written = written
            .checked_add(u64::try_from(chunk.len()).map_err(|_| {
                ApiError::BadRequest("provider driver archive chunk is too large".to_string())
            })?)
            .ok_or_else(|| {
                ApiError::BadRequest("provider driver archive size overflowed".to_string())
            })?;
        if written > expected_bytes {
            return Err(ApiError::BadRequest(format!(
                "provider driver archive exceeds its declared {expected_bytes} bytes"
            )));
        }
        file.write_all(&chunk).await.map_err(|error| {
            ApiError::InternalError(format!(
                "cannot write provider driver upload staging file: {error}"
            ))
        })?;
    }
    Ok(written)
}

fn multipart_error(error: axum::extract::multipart::MultipartError) -> ApiError {
    ApiError::BadRequest(format!(
        "invalid provider driver release multipart body: {error}"
    ))
}

fn credential_material(credential: String) -> ApiResult<CredentialMaterial> {
    CredentialMaterial::new(credential.into_bytes())
        .map_err(|error| ApiError::BadRequest(error.to_string()))
}

fn parse_digest(value: &str, label: &str) -> ApiResult<Sha256Digest> {
    value
        .parse()
        .map_err(|error| ApiError::BadRequest(format!("invalid {label} digest: {error}")))
}

/// Turn the caller's `Idempotency-Key` and the canonical request facts into
/// the record every provider-management mutation shares. `None` means the
/// route treats the key as optional and the caller sent none, so the
/// mutation runs without a replay record.
///
/// The request digest is keyed by a subkey of the secrets master key, so a
/// key that arrives while no master key is usable is refused as
/// `not-configured` naming `secret_key_file`: the mutation must not run
/// unrecorded, and a weaker digest is not an option (see
/// [`crate::catalog::idempotency`]).
fn idempotent_request(
    secrets: &SecretsBackend,
    route_id: &'static str,
    idempotency_key: Option<&str>,
    canonical_request: serde_json::Value,
) -> ApiResult<Option<IdempotentRequest>> {
    idempotency_key
        .map(|idempotency_key| {
            let digest_key =
                IdempotencyDigestKey::derive(secrets).map_err(|error| match error {
                    CatalogError::SecretsUnavailable(reason) => ApiError::NotConfigured(format!(
                        "{route_id} cannot record its Idempotency-Key: {reason}"
                    )),
                    other => other.into(),
                })?;
            let canonical = serde_jcs::to_vec(&canonical_request).map_err(|error| {
                ApiError::InternalError(format!(
                    "cannot canonicalize {route_id} request for its idempotency record: {error}"
                ))
            })?;
            IdempotentRequest::new(route_id, idempotency_key, &canonical, &digest_key)
                .map_err(Into::into)
        })
        .transpose()
}

/// Run one provider-management mutation under the shared idempotency record.
///
/// `mutation` builds the store call and is invoked only when no completed
/// record exists. It receives the pending record (`None` when the route's
/// key is optional and absent) and must hand it to the store, which
/// completes it in the same transaction as the resource write: the
/// resource and its replay reference are one atomic commit, so a crash or
/// a client that disconnects mid-request can never leave a resource behind
/// that its own retry cannot find. A replay answers from `reload` with the
/// original status, so the response is rebuilt from the resource store and
/// never from a stored body. A mutation that fails rolled its transaction
/// back, record included, and the pending row is dropped here, which is
/// what lets a caller fix a stale `If-Match` and retry with the same key.
async fn idempotent_mutation<T, M, MF, R, RF, E, RE>(
    state: &CatalogState,
    principal: &Principal,
    request: Option<IdempotentRequest>,
    success_status: StatusCode,
    location: impl Fn(&T) -> String,
    mutation: M,
    reload: R,
) -> ApiResult<(StatusCode, HeaderMap, Json<T>)>
where
    T: IdempotentResource,
    M: FnOnce(Option<PendingIdempotencyRecord>) -> MF,
    MF: Future<Output = Result<T, E>>,
    E: Into<ApiError>,
    R: FnOnce(CanonicalId) -> RF,
    RF: Future<Output = Result<Option<T>, RE>>,
    RE: Into<ApiError>,
{
    let Some(request) = request else {
        let resource = mutation(None).await.map_err(Into::into)?;
        let headers = resource_headers(&location(&resource), resource.resource_revision())?;
        return Ok((success_status, headers, Json(resource)));
    };
    let records = IdempotencyStore::new(state.store.database());
    let scope = IdempotencyScope {
        actor_subject: actor_subject(principal),
    };
    match records.begin(scope, &request).await? {
        IdempotencyOutcome::Replay(stored) => {
            let resource = reload(stored.resource_id)
                .await
                .map_err(Into::into)?
                .ok_or_else(|| {
                    tracing::error!(
                        route_id = request.route_id(),
                        record_id = %stored.record_id,
                        resource_kind = %stored.resource_kind,
                        resource_id = %stored.resource_id,
                        "Idempotency record references a resource that no longer exists; the replay cannot be answered"
                    );
                    ApiError::NotFound(format!(
                        "{} {} produced by the original request with this Idempotency-Key no longer exists",
                        stored.resource_kind, stored.resource_id
                    ))
                })?;
            let status = StatusCode::from_u16(stored.response_status).map_err(|error| {
                ApiError::InternalError(format!(
                    "idempotency record {} holds an invalid status: {error}",
                    stored.record_id
                ))
            })?;
            let headers = resource_headers(&location(&resource), resource.resource_revision())?;
            Ok((status, headers, Json(resource)))
        }
        IdempotencyOutcome::Fresh(guard) => {
            let record = guard.pending_record(success_status.as_u16());
            match mutation(Some(record)).await {
                Ok(resource) => {
                    let headers =
                        resource_headers(&location(&resource), resource.resource_revision())?;
                    Ok((success_status, headers, Json(resource)))
                }
                Err(error) => {
                    let record_id = guard.record_id();
                    if let Err(abandon_error) = records.abandon(guard).await {
                        tracing::warn!(
                            route_id = request.route_id(),
                            %record_id,
                            error = %abandon_error,
                            "Failed mutation left its pending idempotency record behind; the key answers idempotency-request-in-progress until the record expires ({} s)",
                            crate::catalog::idempotency::PENDING_RECORD_TTL_SECONDS
                        );
                    }
                    Err(error.into())
                }
            }
        }
    }
}

fn credential_mutation_context(
    principal: &Principal,
    idempotency_record: Option<PendingIdempotencyRecord>,
) -> ProviderCredentialMutationContext {
    ProviderCredentialMutationContext {
        actor_subject: actor_subject(principal),
        idempotency_record,
    }
}

fn provider_target_grant_mutation_context(
    principal: &Principal,
    idempotency_record: Option<PendingIdempotencyRecord>,
) -> ProviderTargetGrantMutationContext {
    ProviderTargetGrantMutationContext {
        actor_subject: actor_subject(principal),
        idempotency_record,
    }
}

fn lifecycle_mutation_context(
    principal: &Principal,
    idempotency_record: Option<PendingIdempotencyRecord>,
) -> ProviderLifecycleMutationContext {
    ProviderLifecycleMutationContext {
        actor_subject: actor_subject(principal),
        idempotency_record,
    }
}

/// The durable replay context of a product operation request. `route` is
/// the canonical (`/api`) path of the mutation, which the fingerprint binds
/// with the method and contract version so one key cannot replay another
/// route; the surface the caller used does not change the operation.
fn operation_context(
    headers: &HeaderMap,
    principal: &Principal,
    route: &str,
) -> ApiResult<OperationRequestContext> {
    let idempotency_key = required_header(headers, IDEMPOTENCY_KEY)?;
    keyed_operation_context(headers, principal, route, idempotency_key)
}

fn keyed_operation_context(
    headers: &HeaderMap,
    principal: &Principal,
    route: &str,
    idempotency_key: &str,
) -> ApiResult<OperationRequestContext> {
    let request_id =
        optional_canonical_header(headers, REQUEST_ID)?.unwrap_or_else(CanonicalId::new_v7);
    let correlation_id = optional_canonical_header(headers, CORRELATION_ID)?.unwrap_or(request_id);
    OperationRequestContext::new(
        idempotency_key,
        &format!("POST\n{route}\nv1"),
        actor_subject(principal),
        request_id,
        correlation_id,
    )
    .map_err(Into::into)
}

fn operation_headers(
    paths: ApiPaths,
    operation: &ProductOperation,
    app_name: &str,
    environment_name: &str,
) -> ApiResult<HeaderMap> {
    let mut headers = resource_headers(
        &paths.absolute(&format!(
            "/apps/{app_name}/environments/{environment_name}/provider-deployments/{}",
            operation.operation_id
        )),
        operation.revision,
    )?;
    headers.insert(
        REQUEST_ID,
        HeaderValue::from_str(&operation.request_id.to_string())
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
    headers.insert(
        CORRELATION_ID,
        HeaderValue::from_str(&operation.correlation_id.to_string())
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
    Ok(headers)
}

/// The headers of one provider resource answer. `location` is the absolute
/// path of the resource, already spelled for the caller's surface by
/// [`ApiPaths::absolute`], or empty for a read that is the resource itself.
fn resource_headers(location: &str, revision: u64) -> ApiResult<HeaderMap> {
    let mut headers = HeaderMap::new();
    if !location.is_empty() {
        headers.insert(
            header::LOCATION,
            HeaderValue::from_str(location)
                .map_err(|error| ApiError::InternalError(error.to_string()))?,
        );
    }
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{revision}\""))
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(headers)
}

fn no_store_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers
}

fn parse_id(value: &str, label: &str) -> ApiResult<CanonicalId> {
    value
        .parse()
        .map_err(|error| ApiError::BadRequest(format!("invalid {label} identity: {error}")))
}

fn optional_canonical_header(headers: &HeaderMap, name: &str) -> ApiResult<Option<CanonicalId>> {
    single_header(headers, name)?
        .map(|value| {
            let value = value.to_str().map_err(|_| {
                ApiError::BadRequest(format!("{name} header is not valid visible ASCII"))
            })?;
            parse_id(value, name)
        })
        .transpose()
}

fn required_if_match(headers: &HeaderMap) -> ApiResult<u64> {
    let value = single_header(headers, IF_MATCH)?
        .ok_or_else(|| {
            ApiError::PreconditionRequired(
                "If-Match with the current resource ETag is required".to_string(),
            )
        })?
        .to_str()
        .map_err(|_| ApiError::BadRequest("If-Match is not valid visible ASCII".to_string()))?;
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| {
            ApiError::BadRequest(
                "If-Match must contain one strong, quoted resource revision".to_string(),
            )
        })?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ApiError::BadRequest(
            "If-Match must contain one strong, quoted resource revision".to_string(),
        ));
    }
    value.parse().map_err(|_| {
        ApiError::BadRequest("If-Match revision is outside the supported range".to_string())
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;
    use crate::api::catalog::tests::harness;
    use crate::catalog::provider_driver_release::tests::Fixture as ReleaseFixture;
    use deis_provider_driver::ExecutableProviderDriverConfig;

    async fn response_json(response: axum::response::Response) -> Value {
        let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    /// A JSON request carrying a fresh `Idempotency-Key`, which every create
    /// on this surface requires.
    fn json_request(method: Method, uri: &str, value: Value) -> Request<Body> {
        keyed_json_request(
            method,
            uri,
            &format!("test-{}", CanonicalId::new_v7()),
            value,
        )
    }

    fn keyed_json_request(method: Method, uri: &str, key: &str, value: Value) -> Request<Body> {
        let mut request = json_request_without_key(method, uri, value);
        request
            .headers_mut()
            .insert(IDEMPOTENCY_KEY, HeaderValue::from_str(key).unwrap());
        request
    }

    fn json_request_without_key(method: Method, uri: &str, value: Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap()
    }

    fn keyed_request(method: Method, uri: &str, key: &str, if_match: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(IDEMPOTENCY_KEY, key)
            .header(header::IF_MATCH, if_match)
            .body(Body::empty())
            .unwrap()
    }

    fn account_router(state: &CatalogState, account: &str) -> axum::Router {
        crate::api::catalog::router("/api", state.clone()).layer(Extension(Principal {
            kind: crate::identity::PrincipalKind::Account,
            account: Some(account.to_string()),
            role: Some(crate::identity::Role::Owner),
            credential_id: Some(format!("token-{account}")),
            authority: crate::identity::Authority::Unscoped,
        }))
    }

    async fn idempotency_record_count(state: &CatalogState) -> i64 {
        state
            .store
            .database()
            .with_connection(|connection| {
                connection
                    .query_row("SELECT count(*) FROM api_idempotency_records", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .map_err(crate::catalog::CatalogError::from)
            })
            .await
            .unwrap()
    }

    /// Rewrite a completed record with an expiry already in the past. Rows
    /// are immutable, so ageing one means replacing it under the same
    /// identity; this stands in for a clock the store does not expose.
    async fn expire_idempotency_records(state: &CatalogState) {
        state
            .store
            .database()
            .with_connection(|connection| {
                let transaction = connection.unchecked_transaction()?;
                transaction.execute_batch(
                    "CREATE TEMP TABLE aged AS SELECT * FROM api_idempotency_records;
                     DELETE FROM api_idempotency_records;
                     INSERT INTO api_idempotency_records
                        (record_id, organization_id, actor_id, route_id, idempotency_key_sha256,
                         request_sha256, state, response_status, resource_kind, resource_id,
                         resource_revision, created_at, completed_at, expires_at)
                     SELECT record_id, organization_id, actor_id, route_id, idempotency_key_sha256,
                            request_sha256, state, response_status, resource_kind, resource_id,
                            resource_revision, 1, completed_at - created_at + 1, 2
                       FROM aged;
                     DROP TABLE aged;",
                )?;
                transaction.commit()?;
                Ok::<_, crate::catalog::CatalogError>(())
            })
            .await
            .unwrap();
    }

    fn multipart_request(parts: Vec<(&str, Vec<u8>)>) -> Request<Body> {
        let boundary = "deislet-provider-release-test-boundary";
        let mut body = Vec::new();
        for (name, value) in parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
            );
            let content_type = if name == "archive" {
                "application/x-tar"
            } else {
                "application/json"
            };
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
            body.extend_from_slice(&value);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        Request::builder()
            .method(Method::POST)
            .uri("/api/providers/driver-releases")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap()
    }

    async fn release_router(
        fixture: &ReleaseFixture,
    ) -> (crate::api::catalog::tests::Harness, axum::Router) {
        let harness = harness(true).await;
        let registry = ProviderDriverReleaseRegistry::new(
            harness.state.store.as_ref().clone(),
            &fixture.install_root,
            fixture.trust_root.clone(),
            None,
        )
        .await
        .unwrap();
        let dispatcher = crate::catalog::ProviderDriverDispatcher::new(
            registry,
            Arc::clone(&harness.state.provider_credentials),
            ExecutableProviderDriverConfig::default(),
        )
        .unwrap();
        let mut state = harness.state.clone();
        state.provider_driver_dispatcher = Some(Arc::new(dispatcher));
        let router =
            crate::api::catalog::router("/api", state).layer(Extension(Principal::anonymous()));
        (harness, router)
    }

    fn release_parts(fixture: &ReleaseFixture, archive: Vec<u8>) -> Vec<(&str, Vec<u8>)> {
        vec![
            ("release", fixture.release.canonical_json().unwrap()),
            ("manifest", fixture.manifest.canonical_json().unwrap()),
            (
                "authorization",
                fixture.authorization.canonical_json().unwrap(),
            ),
            ("transparency", fixture.transparency.canonical_json()),
            ("archive", archive),
        ]
    }

    fn upload_staging_files(fixture: &ReleaseFixture) -> Vec<String> {
        fs::read_dir(&fixture.install_root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(RELEASE_UPLOAD_PREFIX))
            .collect()
    }

    #[tokio::test]
    async fn release_api_streams_exact_archive_and_replays_content_addressed_install() {
        let fixture = ReleaseFixture::new();
        let (_harness, router) = release_router(&fixture).await;
        let archive = fs::read(&fixture.archive).unwrap();
        let response = router
            .clone()
            .oneshot(multipart_request(release_parts(&fixture, archive.clone())))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let created = response_json(response).await;
        let release_digest = created["releaseDigest"].as_str().unwrap();
        assert_eq!(created["archiveSizeBytes"], archive.len());
        assert!(upload_staging_files(&fixture).is_empty());

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/providers/driver-releases")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let listed = response_json(response).await;
        assert_eq!(listed["page"]["total"], 1);
        assert_eq!(listed["items"][0]["releaseDigest"], release_digest);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/providers/driver-releases/{release_digest}/audit"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let audit = response_json(response).await;
        assert_eq!(audit["page"]["total"], 1);
        assert_eq!(audit["items"][0]["eventType"], "registered");

        let response = router
            .clone()
            .oneshot(multipart_request(release_parts(&fixture, archive)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response_json(response).await["releaseDigest"],
            release_digest
        );
        assert!(upload_staging_files(&fixture).is_empty());

        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/providers/driver-releases/{release_digest}/audit"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let audit = response_json(response).await;
        assert_eq!(audit["page"]["total"], 2);
        assert_eq!(audit["items"][0]["eventType"], "registered");
        assert_eq!(audit["items"][1]["eventType"], "verified");
    }

    #[tokio::test]
    async fn release_api_rejects_wrong_archive_sizes_and_cleans_staging() {
        let fixture = ReleaseFixture::new();
        let (_harness, router) = release_router(&fixture).await;
        let mut archive = fs::read(&fixture.archive).unwrap();
        archive.pop();
        let response = router
            .clone()
            .oneshot(multipart_request(release_parts(&fixture, archive)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            response_json(response).await["error"]
                .as_str()
                .unwrap()
                .contains("release declares")
        );
        assert!(upload_staging_files(&fixture).is_empty());

        let mut archive = fs::read(&fixture.archive).unwrap();
        archive.push(0);
        let response = router
            .oneshot(multipart_request(release_parts(&fixture, archive)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            response_json(response).await["error"]
                .as_str()
                .unwrap()
                .contains("exceeds its declared")
        );
        assert!(upload_staging_files(&fixture).is_empty());
    }

    #[tokio::test]
    async fn release_api_is_explicitly_disabled_without_a_verified_registry() {
        let harness = harness(true).await;
        let response = harness
            .router
            .oneshot(multipart_request(Vec::new()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            response_json(response).await["error"]
                .as_str()
                .unwrap()
                .contains("release management is disabled")
        );
    }

    /// The request digest is keyed by the secrets master key, so a keyed
    /// create without one is refused before any mutation runs, as a 409
    /// `not-configured` that names the setting to fix.
    #[tokio::test]
    async fn keyed_mutations_are_refused_as_not_configured_without_a_master_key() {
        let harness = harness(false).await;
        let response = harness
            .router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/providers/installations",
                json!({
                    "backendId": "deislet",
                    "accountReference": "account",
                    "credentialBindingId": CanonicalId::new_v7().to_string(),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        // The legacy `/api` body carries only the message; the structured
        // code travels in the extension the versioned surfaces render.
        let detail = response
            .extensions()
            .get::<crate::api::error::ErrorDetail>()
            .cloned()
            .unwrap();
        assert_eq!(detail.code, crate::api::error::codes::NOT_CONFIGURED);
        assert!(
            detail.message.contains("secret_key_file"),
            "{}",
            detail.message
        );
        assert!(
            detail.message.contains("Idempotency-Key"),
            "{}",
            detail.message
        );
        let body = response_json(response).await;
        assert!(body["error"].as_str().unwrap().contains("secret_key_file"));
        assert_eq!(idempotency_record_count(&harness.state).await, 0);
    }

    #[tokio::test]
    async fn credential_lifecycle_never_echoes_material_and_is_revision_fenced() {
        let harness = harness(true).await;
        let secret = "provider-token-that-must-never-return";
        let response = harness
            .router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/providers/credentials",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "name": "production",
                    "credential": secret
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()[header::ETAG], "\"1\"");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let created = response_json(response).await;
        assert!(!created.to_string().contains(secret));
        assert!(created.get("credential").is_none());
        let binding_id = created["bindingId"].as_str().unwrap();

        let response = harness
            .router
            .clone()
            .oneshot(json_request(
                Method::POST,
                &format!("/api/providers/credentials/{binding_id}/rotate"),
                json!({"credential": "replacement"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);

        let mut request = json_request(
            Method::POST,
            &format!("/api/providers/credentials/{binding_id}/rotate"),
            json!({"credential": "replacement"}),
        );
        request
            .headers_mut()
            .insert(header::IF_MATCH, HeaderValue::from_static("\"1\""));
        let response = harness.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"2\"");
        let rotated = response_json(response).await;
        assert_eq!(rotated["currentRevision"], 2);
        assert!(!rotated.to_string().contains("replacement"));

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/providers/credentials/{binding_id}/audit"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let audit = response_json(response).await;
        assert_eq!(audit["page"]["total"], 2);
        assert_eq!(audit["items"][0]["eventType"], "provisioned");
        assert_eq!(audit["items"][1]["eventType"], "rotated");
        assert!(!audit.to_string().contains(secret));

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/providers/credentials/{binding_id}"))
                    .header(header::IF_MATCH, "\"2\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"3\"");
        let revoked = response_json(response).await;
        assert_eq!(revoked["status"], "revoked");
    }

    #[tokio::test]
    async fn provider_lists_use_opaque_query_bound_cursors_and_true_totals() {
        let harness = harness(true).await;
        for name in ["account-a", "account-b", "account-c"] {
            let response = harness
                .router
                .clone()
                .oneshot(json_request(
                    Method::POST,
                    "/api/providers/credentials",
                    json!({
                        "backendId": "cloudflare-workers-pages",
                        "name": name,
                        "credential": format!("test-only-{name}")
                    }),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
        }

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/providers/credentials?limit=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let first = response_json(response).await;
        assert_eq!(first["schemaVersion"], 1);
        assert_eq!(first["items"].as_array().unwrap().len(), 2);
        assert_eq!(first["page"]["limit"], 2);
        assert_eq!(first["page"]["count"], 2);
        assert_eq!(first["page"]["total"], 3);
        let cursor = first["page"]["nextCursor"].as_str().unwrap();
        assert_eq!(cursor.len(), 43);
        assert!(!cursor.contains("account"));

        let second_uri = format!("/api/providers/credentials?limit=1&cursor={cursor}");
        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&second_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let second = response_json(response).await;
        assert_eq!(second["items"].as_array().unwrap().len(), 1);
        assert_eq!(second["page"]["limit"], 1);
        assert_eq!(second["page"]["count"], 1);
        assert_eq!(second["page"]["total"], 3);
        assert!(second["page"]["nextCursor"].is_null());
        assert!(
            first["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["bindingId"] != second["items"][0]["bindingId"])
        );

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&second_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_json(response).await["items"], second["items"]);

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/providers/installations?cursor={cursor}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let mut tampered = cursor.as_bytes().to_vec();
        tampered[0] = if tampered[0] == b'a' { b'b' } else { b'a' };
        let tampered = String::from_utf8(tampered).unwrap();
        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/providers/credentials?limit=2&cursor={tampered}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn installation_api_requires_a_vault_binding_and_targets_require_a_verified_release() {
        let harness = harness(true).await;
        let response = harness
            .router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/providers/credentials",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "name": "provider-account",
                    "credential": "test-only-token"
                }),
            ))
            .await
            .unwrap();
        let binding = response_json(response).await;
        let binding_id = binding["bindingId"].as_str().unwrap();

        let response = harness
            .router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/providers/installations",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "accountReference": "account-123",
                    "region": null,
                    "credentialBindingId": binding_id
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let installation = response_json(response).await;
        let installation_id = installation["installationId"].as_str().unwrap();

        let mut manifest: Value = serde_json::from_str(include_str!(
            "../../../../../contracts/v1/fixtures/valid/provider-driver.json"
        ))
        .unwrap();
        let exact_standard = deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION;
        let response = harness
            .router
            .clone()
            .oneshot(json_request(
                Method::POST,
                &format!("/api/providers/installations/{installation_id}/targets"),
                json!({
                    "name": "production",
                    "standardVersion": exact_standard,
                    "targetConfigDigest": "0000000000000000000000000000000000000000000000000000000000000000",
                    "driverManifest": manifest.clone()
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let failure = response_json(response).await;
        assert!(
            failure["error"]
                .as_str()
                .unwrap()
                .contains("does not declare")
        );

        manifest["supportedStandardVersions"] = json!([{
            "contractId": exact_standard.split_once('@').unwrap().0,
            "version": exact_standard,
            "contractDigest": {
                "algorithm": "sha256",
                "value": deis_wire::CURRENT_EDGE_CANON_CONTRACT_SHA256
            }
        }]);
        let response = harness
            .router
            .clone()
            .oneshot(json_request(
                Method::POST,
                &format!("/api/providers/installations/{installation_id}/targets"),
                json!({
                    "name": "production",
                    "standardVersion": exact_standard,
                    "targetConfigDigest": "0000000000000000000000000000000000000000000000000000000000000000",
                    "driverManifest": manifest
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let failure = response_json(response).await;
        assert!(
            failure["error"]
                .as_str()
                .unwrap()
                .contains("matching verified driver release")
        );

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/providers/installations/{installation_id}/targets"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let targets = response_json(response).await;
        assert_eq!(targets["page"]["total"], 0);
    }

    #[tokio::test]
    async fn target_grant_api_is_scoped_audited_revision_fenced_and_revocable() {
        let fixture = ReleaseFixture::new();
        let (harness, router) = release_router(&fixture).await;
        harness.state.store.create_app("demo").await.unwrap();
        harness
            .state
            .store
            .create_environment("demo", "production")
            .await
            .unwrap();
        harness
            .state
            .store
            .create_environment("demo", "preview")
            .await
            .unwrap();

        let archive = fs::read(&fixture.archive).unwrap();
        let response = router
            .clone()
            .oneshot(multipart_request(release_parts(&fixture, archive)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let response = router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/providers/credentials",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "name": "grant-test",
                    "credential": "grant-test-token"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let binding_id = response_json(response).await["bindingId"]
            .as_str()
            .unwrap()
            .to_owned();
        let response = router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/providers/installations",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "accountReference": "grant-account",
                    "credentialBindingId": binding_id
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let installation_id = response_json(response).await["installationId"]
            .as_str()
            .unwrap()
            .to_owned();
        let response = router
            .clone()
            .oneshot(json_request(
                Method::POST,
                &format!("/api/providers/installations/{installation_id}/targets"),
                json!({
                    "name": "grant-target",
                    "standardVersion": deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
                    "targetConfigDigest": "0000000000000000000000000000000000000000000000000000000000000000",
                    "driverManifest": fixture.manifest.get()
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let target_id = response_json(response).await["targetId"]
            .as_str()
            .unwrap()
            .to_owned();

        let response = router
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/apps/demo/environments/production/provider-target-grants",
                "grant-production-001",
                json!({"providerTargetId": target_id, "note": "production publishing"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()[header::ETAG], "\"1\"");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let location = response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned();
        let created = response_json(response).await;
        let grant_id = created["grantId"].as_str().unwrap();
        assert!(location.ends_with(grant_id));
        assert_eq!(created["status"], "active");
        assert_eq!(created["targetName"], "grant-target");

        // The same key and request replay the grant instead of tripping the
        // one-active-grant rule; a different note under the same key is a
        // reused key.
        let response = router
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/apps/demo/environments/production/provider-target-grants",
                "grant-production-001",
                json!({"providerTargetId": target_id, "note": "production publishing"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()[header::LOCATION], location.as_str());
        assert_eq!(response_json(response).await, created);
        let response = router
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/apps/demo/environments/production/provider-target-grants",
                "grant-production-001",
                json!({"providerTargetId": target_id, "note": "another note"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let failure = response_json(response).await;
        let message = failure["error"].as_str().unwrap();
        assert!(message.starts_with("idempotency-key-reused"), "{message}");
        assert!(message.contains(grant_id), "{message}");

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/apps/demo/environments/production/provider-target-grants")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let listed = response_json(response).await;
        assert_eq!(listed["page"]["total"], 1);
        assert_eq!(listed["items"][0]["grantId"], grant_id);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/apps/demo/environments/preview/provider-target-grants/{grant_id}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(&location)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(&location)
                    .header(header::IF_MATCH, "\"2\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        let response = router
            .clone()
            .oneshot(keyed_request(
                Method::DELETE,
                &location,
                "grant-revoke-001",
                "\"1\"",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"2\"");
        let revoked = response_json(response).await;
        assert_eq!(revoked["status"], "revoked");

        // Replaying the revoke with its key answers 200 again although the
        // If-Match it carries is now stale; a new key with that stale
        // revision is a real precondition failure.
        let response = router
            .clone()
            .oneshot(keyed_request(
                Method::DELETE,
                &location,
                "grant-revoke-001",
                "\"1\"",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"2\"");
        assert_eq!(response_json(response).await, revoked);
        let response = router
            .clone()
            .oneshot(keyed_request(
                Method::DELETE,
                &location,
                "grant-revoke-002",
                "\"1\"",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("{location}/audit"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let audit = response_json(response).await;
        assert_eq!(audit["page"]["total"], 2);
        assert_eq!(audit["items"][0]["eventType"], "granted");
        assert_eq!(audit["items"][1]["eventType"], "revoked");

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/apps/demo/environments/production/provider-target-grants?status=all")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_json(response).await["page"]["total"], 1);

        let response = router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/apps/demo/environments/production/provider-target-grants",
                json!({"providerTargetId": target_id, "note": "replacement grant"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let replacement_grant_id = response_json(response).await["grantId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(replacement_grant_id, grant_id);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/apps/demo/environments/production/provider-target-grants?status=all&limit=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let first_page = response_json(response).await;
        assert_eq!(first_page["page"]["total"], 2);
        assert_eq!(first_page["page"]["count"], 1);
        let cursor = first_page["page"]["nextCursor"].as_str().unwrap();

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/apps/demo/environments/production/provider-target-grants?status=active&cursor={cursor}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/apps/demo/environments/production/provider-target-grants?status=all&cursor={cursor}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let second_page = response_json(response).await;
        assert_eq!(second_page["page"]["total"], 2);
        assert_eq!(second_page["page"]["count"], 1);
        assert!(second_page["page"]["nextCursor"].is_null());
        assert_ne!(
            first_page["items"][0]["grantId"],
            second_page["items"][0]["grantId"]
        );

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/apps/demo/environments/production/provider-target-grants?status=unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// The versioned mount as the router builds it: the product routes
    /// nested under `/api/v1`, an anonymous principal where the gate would
    /// have left one, and the request context that names the surface.
    fn v1_router(state: &CatalogState) -> axum::Router {
        axum::Router::new()
            .nest("/api/v1", crate::api::catalog::router("", state.clone()))
            .layer(Extension(Principal::anonymous()))
            .layer(axum::middleware::from_fn_with_state(
                crate::api::versioning::ApiSurface::V1,
                crate::api::versioning::attach_request_context,
            ))
    }

    /// A provider resource created through `/api/v1` is located under
    /// `/api/v1`; through the legacy tree, under `/api`. A v1 client is never
    /// handed a deprecated path to follow.
    #[tokio::test]
    async fn a_provider_resource_location_is_spelled_for_the_surface_the_caller_used() {
        let harness = harness(true).await;
        let v1 = v1_router(&harness.state);
        for (router, uri, name, expected_prefix) in [
            (
                &harness.router,
                "/api/providers/credentials",
                "legacy",
                "/api/providers/credentials/",
            ),
            (
                &v1,
                "/api/v1/providers/credentials",
                "versioned",
                "/api/v1/providers/credentials/",
            ),
        ] {
            let response = router
                .clone()
                .oneshot(json_request(
                    Method::POST,
                    uri,
                    json!({
                        "backendId": "cloudflare-workers-pages",
                        "name": name,
                        "credential": "test-only-token"
                    }),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED, "{uri}");
            let location = response.headers()[header::LOCATION]
                .to_str()
                .unwrap()
                .to_owned();
            let created = response_json(response).await;
            let binding_id = created["bindingId"].as_str().unwrap();
            assert_eq!(location, format!("{expected_prefix}{binding_id}"), "{uri}");
        }
    }

    /// The missing key as a v1 client reads it: its own code and the header
    /// it has to send, while the flat tree keeps the sentence it always
    /// answered. Asserted through the whole router, because the envelope is
    /// rendered by the surface mount rather than by the handler.
    #[tokio::test]
    async fn a_mutation_without_a_key_says_idempotency_key_required_on_v1() {
        use axum::extract::ConnectInfo;

        let (router, _dir) = crate::api::tests::full_router().await;
        let body = json!({
            "backendId": "cloudflare-workers-pages",
            "name": "production",
            "credential": "test-only-token"
        });
        let refuse = |uri: &'static str| {
            let router = router.clone();
            let body = body.clone();
            async move {
                let mut request = json_request_without_key(Method::POST, uri, body);
                request.extensions_mut().insert(ConnectInfo(
                    "127.0.0.1:40000".parse::<std::net::SocketAddr>().unwrap(),
                ));
                let response = router.oneshot(request).await.unwrap();
                let status = response.status();
                (status, response_json(response).await)
            }
        };

        let (status, refused) = refuse("/api/v1/providers/credentials").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
        assert_eq!(refused["error"]["code"], "idempotency-key-required");
        assert_eq!(
            refused["error"]["message"],
            "idempotency-key header is required"
        );
        assert_eq!(refused["error"]["target"]["resource"], "request");
        assert_eq!(refused["error"]["target"]["field"], "Idempotency-Key");
        assert_eq!(refused["error"]["retryable"], false);

        let (status, refused) = refuse("/api/providers/credentials").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
        assert_eq!(refused["error"], "idempotency-key header is required");
        assert_eq!(refused.as_object().unwrap().len(), 1, "{refused}");
    }

    #[tokio::test]
    async fn credential_create_requires_a_key_replays_once_and_refuses_a_changed_request() {
        let harness = harness(true).await;
        let body = json!({
            "backendId": "cloudflare-workers-pages",
            "name": "production",
            "credential": "test-only-token"
        });
        let response = harness
            .router
            .clone()
            .oneshot(json_request_without_key(
                Method::POST,
                "/api/providers/credentials",
                body.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_json(response).await["error"],
            "idempotency-key header is required"
        );
        let response = harness
            .router
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/providers/credentials",
                "short",
                body.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(idempotency_record_count(&harness.state).await, 0);

        let response = harness
            .router
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/providers/credentials",
                "credential-create-001",
                body.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let location = response.headers()[header::LOCATION].clone();
        let created = response_json(response).await;
        let binding_id = created["bindingId"].as_str().unwrap().to_owned();

        // Byte-identical replay: same status, headers and body, no new row
        // and no second "provisioned" audit event.
        let response = harness
            .router
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/providers/credentials",
                "credential-create-001",
                body.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()[header::LOCATION], location);
        assert_eq!(response.headers()[header::ETAG], "\"1\"");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response_json(response).await, created);

        // The same request spelled with another key order is still the same
        // canonical request.
        let response = harness
            .router
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/providers/credentials",
                "credential-create-001",
                json!({
                    "credential": "test-only-token",
                    "name": "production",
                    "backendId": "cloudflare-workers-pages"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response_json(response).await["bindingId"], binding_id);

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/providers/credentials")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response_json(response).await["page"]["total"], 1);
        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/providers/credentials/{binding_id}/audit"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let audit = response_json(response).await;
        assert_eq!(audit["page"]["total"], 1);
        assert_eq!(audit["items"][0]["eventType"], "provisioned");
        assert_eq!(idempotency_record_count(&harness.state).await, 1);

        // Same key, different request: a stable conflict naming the
        // original resource, and nothing new is created.
        for changed in [
            json!({
                "backendId": "cloudflare-workers-pages",
                "name": "staging",
                "credential": "test-only-token"
            }),
            json!({
                "backendId": "cloudflare-workers-pages",
                "name": "production",
                "credential": "another-token"
            }),
        ] {
            let response = harness
                .router
                .clone()
                .oneshot(keyed_json_request(
                    Method::POST,
                    "/api/providers/credentials",
                    "credential-create-001",
                    changed,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);
            let failure = response_json(response).await;
            let message = failure["error"].as_str().unwrap();
            assert!(message.starts_with("idempotency-key-reused"), "{message}");
            assert!(
                message.contains(&format!("provider-credential-binding {binding_id}")),
                "{message}"
            );
            assert!(!message.contains("token"), "{message}");
        }

        // A different key with a clashing name is still the catalog's own
        // conflict, and the failed attempt leaves no record behind.
        let response = harness
            .router
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/providers/credentials",
                "credential-create-002",
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let failure = response_json(response).await;
        assert!(
            failure["error"]
                .as_str()
                .unwrap()
                .contains("already exists")
        );
        assert_eq!(idempotency_record_count(&harness.state).await, 1);
    }

    #[tokio::test]
    async fn idempotency_keys_are_scoped_to_the_principal() {
        let harness = harness(true).await;
        let alice = account_router(&harness.state, "alice");
        let bob = account_router(&harness.state, "bob");
        let response = alice
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/providers/credentials",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "name": "shared",
                    "credential": "test-only-token"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let binding_id = response_json(response).await["bindingId"]
            .as_str()
            .unwrap()
            .to_owned();

        let response = alice
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/providers/installations",
                "install-001",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "accountReference": "alice-account",
                    "credentialBindingId": binding_id
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let alice_installation = response_json(response).await["installationId"]
            .as_str()
            .unwrap()
            .to_owned();

        // Bob reuses Alice's key with a different body and is neither
        // replayed nor refused: the key was never his.
        let response = bob
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/providers/installations",
                "install-001",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "accountReference": "bob-account",
                    "credentialBindingId": binding_id
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let bob_installation = response_json(response).await["installationId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(alice_installation, bob_installation);

        // Alice's replay still answers with her own installation.
        let response = alice
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/providers/installations",
                "install-001",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "accountReference": "alice-account",
                    "credentialBindingId": binding_id
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response_json(response).await["installationId"],
            alice_installation
        );
        let response = bob
            .oneshot(
                Request::builder()
                    .uri("/api/providers/installations")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response_json(response).await["page"]["total"], 2);
    }

    #[tokio::test]
    async fn credential_rotate_and_revoke_replay_after_success_but_a_stale_if_match_still_fails() {
        let harness = harness(true).await;
        let response = harness
            .router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/providers/credentials",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "name": "rotating",
                    "credential": "first-token"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let binding_id = response_json(response).await["bindingId"]
            .as_str()
            .unwrap()
            .to_owned();
        let rotate_uri = format!("/api/providers/credentials/{binding_id}/rotate");

        // A key on rotate is optional; If-Match alone still works.
        let mut request = json_request_without_key(
            Method::POST,
            &rotate_uri,
            json!({"credential": "second-token"}),
        );
        request
            .headers_mut()
            .insert(header::IF_MATCH, HeaderValue::from_static("\"1\""));
        let response = harness.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"2\"");
        assert_eq!(idempotency_record_count(&harness.state).await, 1);

        let mut request = keyed_json_request(
            Method::POST,
            &rotate_uri,
            "rotate-001",
            json!({"credential": "third-token"}),
        );
        request
            .headers_mut()
            .insert(header::IF_MATCH, HeaderValue::from_static("\"2\""));
        let response = harness.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"3\"");
        let rotated = response_json(response).await;
        assert_eq!(rotated["currentRevision"], 3);

        // The replay carries the now-stale If-Match "2" and still succeeds
        // with the same response; no second rotation happens.
        let mut request = keyed_json_request(
            Method::POST,
            &rotate_uri,
            "rotate-001",
            json!({"credential": "third-token"}),
        );
        request
            .headers_mut()
            .insert(header::IF_MATCH, HeaderValue::from_static("\"2\""));
        let response = harness.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"3\"");
        assert_eq!(response_json(response).await, rotated);

        // Same key, different credential: refused rather than replayed.
        let mut request = keyed_json_request(
            Method::POST,
            &rotate_uri,
            "rotate-001",
            json!({"credential": "fourth-token"}),
        );
        request
            .headers_mut()
            .insert(header::IF_MATCH, HeaderValue::from_static("\"2\""));
        let response = harness.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let message = response_json(response).await["error"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(message.starts_with("idempotency-key-reused"), "{message}");
        assert!(!message.contains("token"), "{message}");

        // A genuinely stale If-Match under a new key is a 412 and leaves no
        // record, so the corrected retry may reuse that key.
        let mut request = keyed_json_request(
            Method::POST,
            &rotate_uri,
            "rotate-002",
            json!({"credential": "fourth-token"}),
        );
        request
            .headers_mut()
            .insert(header::IF_MATCH, HeaderValue::from_static("\"2\""));
        let response = harness.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(idempotency_record_count(&harness.state).await, 2);
        let mut request = keyed_json_request(
            Method::POST,
            &rotate_uri,
            "rotate-002",
            json!({"credential": "fourth-token"}),
        );
        request
            .headers_mut()
            .insert(header::IF_MATCH, HeaderValue::from_static("\"3\""));
        let response = harness.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"4\"");

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/providers/credentials/{binding_id}/audit"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let audit = response_json(response).await;
        assert_eq!(audit["page"]["total"], 4);
        assert!(!audit.to_string().contains("token"));

        let revoke_uri = format!("/api/providers/credentials/{binding_id}");
        let response = harness
            .router
            .clone()
            .oneshot(keyed_request(
                Method::DELETE,
                &revoke_uri,
                "revoke-001",
                "\"4\"",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"5\"");
        let revoked = response_json(response).await;
        assert_eq!(revoked["status"], "revoked");
        let response = harness
            .router
            .clone()
            .oneshot(keyed_request(
                Method::DELETE,
                &revoke_uri,
                "revoke-001",
                "\"4\"",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"5\"");
        assert_eq!(response_json(response).await, revoked);
        let response = harness
            .router
            .clone()
            .oneshot(keyed_request(
                Method::DELETE,
                &revoke_uri,
                "revoke-002",
                "\"4\"",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn expired_idempotency_records_are_treated_as_fresh_requests() {
        let harness = harness(true).await;
        let create = json!({
            "backendId": "cloudflare-workers-pages",
            "name": "expiring",
            "credential": "test-only-token"
        });
        let response = harness
            .router
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/providers/credentials",
                "create-expiring-001",
                create.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let binding_id = response_json(response).await["bindingId"]
            .as_str()
            .unwrap()
            .to_owned();
        let rotate_uri = format!("/api/providers/credentials/{binding_id}/rotate");
        let rotate = || {
            let mut request = keyed_json_request(
                Method::POST,
                &rotate_uri,
                "rotate-expiring-001",
                json!({"credential": "second-token"}),
            );
            request
                .headers_mut()
                .insert(header::IF_MATCH, HeaderValue::from_static("\"1\""));
            request
        };
        let response = harness.router.clone().oneshot(rotate()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"2\"");
        let response = harness.router.clone().oneshot(rotate()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], "\"2\"");
        assert_eq!(idempotency_record_count(&harness.state).await, 2);

        expire_idempotency_records(&harness.state).await;

        // With its record gone the rotate is evaluated afresh: its If-Match
        // is stale, so it fails, and the failed attempt leaves no record.
        let response = harness.router.clone().oneshot(rotate()).await.unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(idempotency_record_count(&harness.state).await, 0);

        // The create is evaluated afresh too: the catalog's own name
        // conflict answers instead of a replay of the original binding.
        let response = harness
            .router
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/providers/credentials",
                "create-expiring-001",
                create,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let message = response_json(response).await["error"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(message.contains("already exists"), "{message}");
        assert!(!message.starts_with("idempotency-key-reused"), "{message}");
        assert_eq!(idempotency_record_count(&harness.state).await, 0);
    }

    #[tokio::test]
    async fn provider_deployment_rejects_a_missing_idempotency_key_before_catalog_work() {
        let harness = harness(true).await;
        harness.state.store.create_app("demo").await.unwrap();
        harness
            .state
            .store
            .create_environment("demo", "production")
            .await
            .unwrap();
        let response = harness
            .router
            .clone()
            .oneshot(json_request_without_key(
                Method::POST,
                "/api/apps/demo/environments/production/provider-deployments",
                json!({
                    "providerTargetId": CanonicalId::new_v7(),
                    "artifact": {
                        "artifactVersionId": CanonicalId::new_v7(),
                        "canonicalArtifactSha256": "0000000000000000000000000000000000000000000000000000000000000000",
                        "providerPayloadSha256": "1111111111111111111111111111111111111111111111111111111111111111"
                    },
                    "plan": {}
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let failure = response_json(response).await;
        assert_eq!(failure["error"], "idempotency-key header is required");

        let missing_target = CanonicalId::new_v7();
        let mut request = json_request_without_key(
            Method::POST,
            "/api/apps/demo/environments/production/provider-deployments",
            json!({
                "providerTargetId": missing_target,
                "artifact": {
                    "artifactVersionId": CanonicalId::new_v7(),
                    "canonicalArtifactSha256": "0000000000000000000000000000000000000000000000000000000000000000",
                    "providerPayloadSha256": "1111111111111111111111111111111111111111111111111111111111111111"
                },
                "plan": {}
            }),
        );
        request.headers_mut().insert(
            IDEMPOTENCY_KEY,
            HeaderValue::from_static("deploy-request-001"),
        );
        let response = harness.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let failure = response_json(response).await;
        assert_eq!(
            failure["error"],
            "provider target is not granted to this application/environment"
        );
        assert!(!failure.to_string().contains(&missing_target.to_string()));

        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .uri("/api/apps/demo/environments/production/provider-deployments")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let deployments = response_json(response).await;
        assert_eq!(deployments["page"]["total"], 0);
    }

    /// A verified release, a credential, an installation, one target and a
    /// grant to `demo/production`, all created through the public API. Every
    /// lifecycle test below starts from this platform.
    struct ProviderFixture {
        harness: crate::api::catalog::tests::Harness,
        router: axum::Router,
        installation_id: String,
        target_id: String,
        grant_id: String,
    }

    async fn provider_fixture(fixture: &ReleaseFixture) -> ProviderFixture {
        let (harness, router) = release_router(fixture).await;
        harness.state.store.create_app("demo").await.unwrap();
        for environment in ["production", "preview"] {
            harness
                .state
                .store
                .create_environment("demo", environment)
                .await
                .unwrap();
        }
        let archive = fs::read(&fixture.archive).unwrap();
        let response = router
            .clone()
            .oneshot(multipart_request(release_parts(fixture, archive)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let response = router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/providers/credentials",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "name": "lifecycle-test",
                    "credential": "lifecycle-test-token"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let binding_id = response_json(response).await["bindingId"]
            .as_str()
            .unwrap()
            .to_owned();
        let response = router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/providers/installations",
                json!({
                    "backendId": "cloudflare-workers-pages",
                    "accountReference": "lifecycle-account",
                    "credentialBindingId": binding_id
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let installation_id = response_json(response).await["installationId"]
            .as_str()
            .unwrap()
            .to_owned();
        let target_id = register_target(&router, fixture, &installation_id, "production").await;
        let grant_id = grant(&router, "production", &target_id).await;
        ProviderFixture {
            harness,
            router,
            installation_id,
            target_id,
            grant_id,
        }
    }

    async fn register_target(
        router: &axum::Router,
        fixture: &ReleaseFixture,
        installation_id: &str,
        name: &str,
    ) -> String {
        let response = router
            .clone()
            .oneshot(json_request(
                Method::POST,
                &format!("/api/providers/installations/{installation_id}/targets"),
                json!({
                    "name": name,
                    "standardVersion": deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
                    "targetConfigDigest": "0000000000000000000000000000000000000000000000000000000000000000",
                    "driverManifest": fixture.manifest.get()
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        response_json(response).await["targetId"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn grant(router: &axum::Router, environment: &str, target_id: &str) -> String {
        let response = router
            .clone()
            .oneshot(json_request(
                Method::POST,
                &format!("/api/apps/demo/environments/{environment}/provider-target-grants"),
                json!({"providerTargetId": target_id}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        response_json(response).await["grantId"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// One canonical artifact with a Cloudflare derivation and a version
    /// pointing at it, so a provider deployment passes lineage validation.
    async fn attach_artifact(harness: &crate::api::catalog::tests::Harness) -> Value {
        let artifact_id = CanonicalId::new_v7();
        let artifact_version_id = CanonicalId::new_v7();
        let standard_version = deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned();
        harness
            .state
            .store
            .database()
            .with_connection(move |connection| {
                let now = crate::catalog::now_secs();
                let app_id: String =
                    connection.query_row("SELECT id FROM apps WHERE id = 'demo'", [], |row| {
                        row.get(0)
                    })?;
                connection.execute(
                    "INSERT INTO canonical_artifacts
                       (id, app_id, artifact_sha256, content_root_sha256,
                        standard_version, file_count, storage_path, record, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 1, '/test/canonical', '{}', ?6)",
                    rusqlite::params![
                        artifact_id.to_string(),
                        app_id,
                        "1".repeat(64),
                        "3".repeat(64),
                        standard_version,
                        now,
                    ],
                )?;
                for (manifest_sha256, provider, payload) in [
                    ("4".repeat(64), "deislet", "5".repeat(64)),
                    ("6".repeat(64), "cloudflare", "2".repeat(64)),
                ] {
                    connection.execute(
                        "INSERT INTO provider_derivations
                           (manifest_sha256, artifact_id, provider, standard_version,
                            payload_root_sha256, manifest, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, '{}', ?6)",
                        rusqlite::params![
                            manifest_sha256,
                            artifact_id.to_string(),
                            provider,
                            standard_version,
                            payload,
                            now,
                        ],
                    )?;
                }
                connection.execute(
                    "INSERT INTO versions
                       (id, canonical_id, app_id, number, bundle_sha, bundle_path,
                        manifest, canonical_artifact_id, provider_derivation_sha256, created_at)
                     VALUES (?1, ?2, ?3, 1, ?4, '/test/bundle.zip', '{}', ?5, ?6, ?7)",
                    rusqlite::params![
                        uuid::Uuid::new_v4().to_string(),
                        artifact_version_id.to_string(),
                        app_id,
                        "7".repeat(64),
                        artifact_id.to_string(),
                        "4".repeat(64),
                        now,
                    ],
                )?;
                Ok::<(), crate::catalog::CatalogError>(())
            })
            .await
            .unwrap();
        json!({
            "artifactVersionId": artifact_version_id,
            "canonicalArtifactSha256": "1".repeat(64),
            "providerPayloadSha256": "2".repeat(64),
        })
    }

    async fn begin_deployment(
        router: &axum::Router,
        target_id: &str,
        artifact: &Value,
        key: &str,
    ) -> (StatusCode, Value) {
        let response = router
            .clone()
            .oneshot(keyed_json_request(
                Method::POST,
                "/api/apps/demo/environments/production/provider-deployments",
                key,
                json!({"providerTargetId": target_id, "artifact": artifact, "plan": {}}),
            ))
            .await
            .unwrap();
        let status = response.status();
        (status, response_json(response).await)
    }

    async fn ready_for_dispatch(harness: &crate::api::catalog::tests::Harness) -> Vec<String> {
        harness
            .state
            .store
            .provider_deployments_ready_for_dispatch(
                chrono::Utc::now(),
                std::time::Duration::from_secs(30),
                10,
            )
            .await
            .unwrap()
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    /// A GET whose body may not be JSON: an extractor rejection (an unknown
    /// query field, say) answers in plain text, which reads as `Null` here.
    async fn get(router: &axum::Router, uri: &str) -> (StatusCode, Value) {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn mutate(
        router: &axum::Router,
        method: Method,
        uri: &str,
        key: &str,
        if_match: &str,
    ) -> (StatusCode, HeaderMap, Value) {
        let response = router
            .clone()
            .oneshot(keyed_request(method, uri, key, if_match))
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        (status, headers, response_json(response).await)
    }

    #[tokio::test]
    async fn installation_disable_fails_closed_for_new_work_and_leaves_in_flight_work_untouched() {
        let fixture = ReleaseFixture::new();
        let seeded = provider_fixture(&fixture).await;
        let router = &seeded.router;
        let artifact = attach_artifact(&seeded.harness).await;
        let (status, operation) = begin_deployment(
            router,
            &seeded.target_id,
            &artifact,
            "deploy-before-disable",
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{operation}");
        let operation_id = operation["operationId"].as_str().unwrap().to_owned();
        assert_eq!(
            ready_for_dispatch(&seeded.harness).await,
            std::slice::from_ref(&operation_id)
        );
        let installation_uri = format!("/api/providers/installations/{}", seeded.installation_id);

        // If-Match is mandatory, exactly as for rotate/revoke.
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("{installation_uri}/disable"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);

        let (status, headers, disabled) = mutate(
            router,
            Method::POST,
            &format!("{installation_uri}/disable"),
            "installation-disable-001",
            "\"1\"",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{disabled}");
        assert_eq!(headers[header::ETAG], "\"2\"");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(disabled["status"], "disabled");
        assert!(disabled["deletedAt"].is_null());

        // The dispatcher no longer selects the workflow, but the workflow
        // itself is exactly as it was.
        assert!(ready_for_dispatch(&seeded.harness).await.is_empty());
        let (status, workflow) = get(
            router,
            &format!("/api/apps/demo/environments/production/provider-deployments/{operation_id}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(workflow["operation"]["state"], "queued");
        assert_eq!(workflow["nextStepIndex"], 0);

        // The target reports why an `active` target is unusable.
        let (status, target) = get(
            router,
            &format!("/api/providers/targets/{}", seeded.target_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(target["status"], "active");
        assert_eq!(target["installationStatus"], "disabled");

        // New work fails closed: a deployment under a fresh key and a grant
        // on another environment are both refused.
        let (status, failure) = begin_deployment(
            router,
            &seeded.target_id,
            &artifact,
            "deploy-while-disabled",
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{failure}");
        let response = router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/apps/demo/environments/preview/provider-target-grants",
                json!({"providerTargetId": seeded.target_id}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);

        // The disable replays under its key with the stale If-Match.
        let (status, headers, replayed) = mutate(
            router,
            Method::POST,
            &format!("{installation_uri}/disable"),
            "installation-disable-001",
            "\"1\"",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::ETAG], "\"2\"");
        assert_eq!(replayed, disabled);

        let (status, headers, enabled) = mutate(
            router,
            Method::POST,
            &format!("{installation_uri}/enable"),
            "installation-enable-001",
            "\"2\"",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{enabled}");
        assert_eq!(headers[header::ETAG], "\"3\"");
        assert_eq!(enabled["status"], "active");
        assert_eq!(
            ready_for_dispatch(&seeded.harness).await,
            std::slice::from_ref(&operation_id)
        );
        let (status, deployed) =
            begin_deployment(router, &seeded.target_id, &artifact, "deploy-after-enable").await;
        // The environment already has a live workflow on this target; that
        // conflict, not the target's status, is what answers now.
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            deployed["error"]
                .as_str()
                .unwrap()
                .contains("already has active deployment operation"),
            "{deployed}"
        );

        let (status, audit) = get(router, &format!("{installation_uri}/audit")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(audit["page"]["total"], 2);
        assert_eq!(audit["items"][0]["eventType"], "disabled");
        assert_eq!(audit["items"][0]["resourceRevision"], 2);
        assert_eq!(audit["items"][0]["reason"], "administrator");
        assert_eq!(audit["items"][0]["cascade"], json!([]));
        assert_eq!(audit["items"][1]["eventType"], "enabled");
        assert_eq!(audit["items"][1]["resourceRevision"], 3);
    }

    #[tokio::test]
    async fn installation_delete_refuses_dependents_then_cascades_with_audit() {
        let fixture = ReleaseFixture::new();
        let seeded = provider_fixture(&fixture).await;
        let router = &seeded.router;
        let installation_uri = format!("/api/providers/installations/{}", seeded.installation_id);
        let records_before = idempotency_record_count(&seeded.harness.state).await;

        let (status, _, refused) = mutate(
            router,
            Method::DELETE,
            &installation_uri,
            "installation-delete-001",
            "\"1\"",
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let message = refused["error"].as_str().unwrap();
        assert!(
            message.starts_with("provider-installation-has-dependents"),
            "{message}"
        );
        assert!(message.contains(&seeded.target_id), "{message}");
        assert!(message.contains(&seeded.grant_id), "{message}");
        // The refused delete left no replay record behind.
        assert_eq!(
            idempotency_record_count(&seeded.harness.state).await,
            records_before
        );

        let (status, headers, deleted) = mutate(
            router,
            Method::DELETE,
            &format!("{installation_uri}?cascade=true"),
            "installation-delete-002",
            "\"1\"",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{deleted}");
        assert_eq!(headers[header::ETAG], "\"2\"");
        assert_eq!(deleted["status"], "revoked");
        assert!(deleted["deletedAt"].is_string());

        let (status, target) = get(
            router,
            &format!("/api/providers/targets/{}", seeded.target_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(target["status"], "disabled");
        assert_eq!(target["installationStatus"], "revoked");
        assert!(target["deletedAt"].is_string());
        assert_eq!(target["revision"], 2);

        let (status, grant) = get(
            router,
            &format!("/api/providers/target-grants/{}", seeded.grant_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(grant["status"], "revoked");
        let (status, grant_audit) = get(
            router,
            &format!("/api/providers/target-grants/{}/audit", seeded.grant_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(grant_audit["page"]["total"], 2);
        assert_eq!(grant_audit["items"][0]["eventType"], "granted");
        assert!(grant_audit["items"][0]["reason"].is_null());
        assert_eq!(grant_audit["items"][1]["eventType"], "revoked");
        assert_eq!(grant_audit["items"][1]["reason"], "installation-deleted");

        let (status, audit) = get(router, &format!("{installation_uri}/audit")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(audit["page"]["total"], 1);
        assert_eq!(audit["items"][0]["eventType"], "deleted");
        assert_eq!(audit["items"][0]["reason"], "administrator");
        assert_eq!(
            audit["items"][0]["cascade"],
            json!([
                {"kind": "provider-target", "id": seeded.target_id},
                {"kind": "provider-target-grant", "id": seeded.grant_id},
            ])
        );
        let (status, target_audit) = get(
            router,
            &format!("/api/providers/targets/{}/audit", seeded.target_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(target_audit["page"]["total"], 1);
        assert_eq!(target_audit["items"][0]["eventType"], "deleted");
        assert_eq!(target_audit["items"][0]["reason"], "installation-deleted");
        assert_eq!(
            target_audit["items"][0]["cascade"],
            json!([{"kind": "provider-target-grant", "id": seeded.grant_id}])
        );

        // The delete replays under its key; under a new key the deleted row
        // answers the same way, and a deleted resource never changes status.
        let (status, headers, replayed) = mutate(
            router,
            Method::DELETE,
            &format!("{installation_uri}?cascade=true"),
            "installation-delete-002",
            "\"1\"",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::ETAG], "\"2\"");
        assert_eq!(replayed, deleted);
        let (status, _, again) = mutate(
            router,
            Method::DELETE,
            &installation_uri,
            "installation-delete-003",
            "\"2\"",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(again, deleted);
        let (status, _, failure) = mutate(
            router,
            Method::POST,
            &format!("{installation_uri}/enable"),
            "installation-enable-001",
            "\"2\"",
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            failure["error"]
                .as_str()
                .unwrap()
                .starts_with("provider-resource-deleted"),
            "{failure}"
        );
        let (status, _, failure) = mutate(
            router,
            Method::POST,
            &format!("/api/providers/targets/{}/enable", seeded.target_id),
            "target-enable-001",
            "\"2\"",
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            failure["error"]
                .as_str()
                .unwrap()
                .starts_with("provider-resource-deleted"),
            "{failure}"
        );

        // History keeps every row: the installation and its target are
        // still listed and readable.
        let (status, installations) = get(router, "/api/providers/installations").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(installations["page"]["total"], 1);
        let (status, targets) = get(router, "/api/providers/targets").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(targets["page"]["total"], 1);
    }

    #[tokio::test]
    async fn target_delete_refuses_while_a_workflow_is_not_terminal_but_disable_is_allowed() {
        let fixture = ReleaseFixture::new();
        let seeded = provider_fixture(&fixture).await;
        let router = &seeded.router;
        let artifact = attach_artifact(&seeded.harness).await;
        let (status, operation) =
            begin_deployment(router, &seeded.target_id, &artifact, "deploy-before-delete").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{operation}");
        let operation_id = operation["operationId"].as_str().unwrap().to_owned();
        let target_uri = format!("/api/providers/targets/{}", seeded.target_id);

        for (uri, key) in [
            (target_uri.clone(), "target-delete-001"),
            (
                format!(
                    "/api/providers/installations/{}?cascade=true",
                    seeded.installation_id
                ),
                "installation-delete-001",
            ),
        ] {
            let (status, _, refused) = mutate(router, Method::DELETE, &uri, key, "\"1\"").await;
            assert_eq!(status, StatusCode::CONFLICT, "{uri}");
            let message = refused["error"].as_str().unwrap();
            assert!(
                message.starts_with("provider-target-in-use")
                    || message.starts_with("provider-installation-in-use"),
                "{message}"
            );
            assert!(message.contains(&operation_id), "{message}");
        }
        // Nothing was cancelled or revoked by the refusals.
        let (_, grant) = get(
            router,
            &format!("/api/providers/target-grants/{}", seeded.grant_id),
        )
        .await;
        assert_eq!(grant["status"], "active");
        let (_, target) = get(router, &target_uri).await;
        assert_eq!(target["revision"], 1);

        // Disabling the target is reversible, so it is allowed with work in
        // flight; the dispatcher just stops selecting that work.
        let (status, headers, disabled) = mutate(
            router,
            Method::POST,
            &format!("{target_uri}/disable"),
            "target-disable-001",
            "\"1\"",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{disabled}");
        assert_eq!(headers[header::ETAG], "\"2\"");
        assert_eq!(disabled["status"], "disabled");
        assert!(ready_for_dispatch(&seeded.harness).await.is_empty());
        let (_, workflow) = get(
            router,
            &format!("/api/apps/demo/environments/production/provider-deployments/{operation_id}"),
        )
        .await;
        assert_eq!(workflow["operation"]["state"], "queued");
        let (status, headers, enabled) = mutate(
            router,
            Method::POST,
            &format!("{target_uri}/enable"),
            "target-enable-001",
            "\"2\"",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{enabled}");
        assert_eq!(headers[header::ETAG], "\"3\"");
        assert_eq!(enabled["status"], "active");
        assert_eq!(ready_for_dispatch(&seeded.harness).await, [operation_id]);
    }

    #[tokio::test]
    async fn target_delete_revokes_its_grants_and_frees_its_installation() {
        let fixture = ReleaseFixture::new();
        let seeded = provider_fixture(&fixture).await;
        let router = &seeded.router;
        let preview_grant_id = grant(router, "preview", &seeded.target_id).await;
        let target_uri = format!("/api/providers/targets/{}", seeded.target_id);

        let (status, headers, deleted) = mutate(
            router,
            Method::DELETE,
            &target_uri,
            "target-delete-001",
            "\"1\"",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{deleted}");
        assert_eq!(headers[header::ETAG], "\"2\"");
        assert_eq!(deleted["status"], "disabled");
        assert!(deleted["deletedAt"].is_string());

        for grant_id in [&seeded.grant_id, &preview_grant_id] {
            let (status, grant) =
                get(router, &format!("/api/providers/target-grants/{grant_id}")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(grant["status"], "revoked");
            let (_, audit) = get(
                router,
                &format!("/api/providers/target-grants/{grant_id}/audit"),
            )
            .await;
            assert_eq!(audit["items"][1]["eventType"], "revoked");
            assert_eq!(audit["items"][1]["reason"], "target-deleted");
        }
        let (_, audit) = get(router, &format!("{target_uri}/audit")).await;
        assert_eq!(audit["page"]["total"], 1);
        assert_eq!(audit["items"][0]["eventType"], "deleted");
        assert_eq!(audit["items"][0]["reason"], "administrator");
        assert_eq!(audit["items"][0]["cascade"].as_array().unwrap().len(), 2);

        // A grant on the deleted target is refused; the environment list
        // still shows the revoked history.
        let response = router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/apps/demo/environments/production/provider-target-grants",
                json!({"providerTargetId": seeded.target_id}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let (_, listed) = get(
            router,
            "/api/apps/demo/environments/production/provider-target-grants?status=revoked",
        )
        .await;
        assert_eq!(listed["page"]["total"], 1);

        // With no living target and no active grant left, the installation
        // deletes without a cascade.
        let (status, _, installation) = mutate(
            router,
            Method::DELETE,
            &format!("/api/providers/installations/{}", seeded.installation_id),
            "installation-delete-001",
            "\"1\"",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{installation}");
        assert_eq!(installation["status"], "revoked");
        let (_, audit) = get(
            router,
            &format!(
                "/api/providers/installations/{}/audit",
                seeded.installation_id
            ),
        )
        .await;
        assert_eq!(audit["items"][0]["cascade"], json!([]));
    }

    #[tokio::test]
    async fn retiring_an_application_revokes_its_grants_with_the_cause_recorded() {
        let fixture = ReleaseFixture::new();
        let seeded = provider_fixture(&fixture).await;
        let router = &seeded.router;
        let preview_grant_id = grant(router, "preview", &seeded.target_id).await;
        let alice = account_router(&seeded.harness.state, "alice");

        let response = alice
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/api/apps/demo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let (status, active) = get(router, "/api/providers/target-grants?status=active").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(active["page"]["total"], 0);
        let (_, revoked) = get(router, "/api/providers/target-grants?status=revoked").await;
        assert_eq!(revoked["page"]["total"], 2);
        for grant_id in [&seeded.grant_id, &preview_grant_id] {
            let (status, grant) =
                get(router, &format!("/api/providers/target-grants/{grant_id}")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(grant["status"], "revoked");
            assert_eq!(grant["revision"], 2);
            let (status, audit) = get(
                router,
                &format!("/api/providers/target-grants/{grant_id}/audit"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(audit["page"]["total"], 2);
            assert_eq!(audit["items"][1]["eventType"], "revoked");
            assert_eq!(audit["items"][1]["reason"], "application-retired");
            // The application path is gone, so only the global history can
            // still answer for this grant.
            let (status, _) = get(
                router,
                &format!(
                    "/api/apps/demo/environments/production/provider-target-grants/{grant_id}/audit"
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        // The revocations name the principal who deleted the application.
        let actor: String = seeded
            .harness
            .state
            .store
            .database()
            .with_connection(move |connection| {
                connection
                    .query_row(
                        "SELECT a.subject FROM provider_target_grants g
                           JOIN operation_actors a ON a.actor_id = g.revoked_by_actor_id
                          WHERE g.status = 'revoked' LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(crate::catalog::CatalogError::from)
            })
            .await
            .unwrap();
        assert_eq!(actor, "account:alice");

        // The target is untouched: an application going away does not
        // change the platform resource it was granted.
        let (_, target) = get(
            router,
            &format!("/api/providers/targets/{}", seeded.target_id),
        )
        .await;
        assert_eq!(target["status"], "active");
        assert_eq!(target["revision"], 1);
    }

    #[tokio::test]
    async fn global_grant_history_pages_with_exact_totals_and_filter_bound_cursors() {
        let fixture = ReleaseFixture::new();
        let seeded = provider_fixture(&fixture).await;
        let router = &seeded.router;
        let second_target =
            register_target(router, &fixture, &seeded.installation_id, "second").await;
        let preview_grant = grant(router, "preview", &seeded.target_id).await;
        let second_grant = grant(router, "production", &second_target).await;
        let (status, _, revoked) = mutate(
            router,
            Method::DELETE,
            &format!("/api/apps/demo/environments/preview/provider-target-grants/{preview_grant}"),
            "grant-revoke-001",
            "\"1\"",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{revoked}");

        let (status, first) = get(router, "/api/providers/target-grants?limit=2").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["schemaVersion"], 1);
        assert_eq!(first["page"]["total"], 3);
        assert_eq!(first["page"]["count"], 2);
        assert_eq!(first["items"][0]["grantId"], second_grant);
        let cursor = first["page"]["nextCursor"].as_str().unwrap().to_owned();

        // The cursor is bound to its filters: a different status or target
        // is a bad request, and so is a cursor from another collection.
        for uri in [
            format!("/api/providers/target-grants?status=active&cursor={cursor}"),
            format!(
                "/api/providers/target-grants?target={}&cursor={cursor}",
                seeded.target_id
            ),
            format!(
                "/api/apps/demo/environments/production/provider-target-grants?status=all&cursor={cursor}"
            ),
        ] {
            let (status, _) = get(router, &uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }
        let (status, second) = get(
            router,
            &format!("/api/providers/target-grants?cursor={cursor}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(second["page"]["total"], 3);
        assert_eq!(second["page"]["count"], 1);
        assert!(second["page"]["nextCursor"].is_null());
        assert_eq!(second["items"][0]["grantId"], seeded.grant_id);

        let (_, active) = get(router, "/api/providers/target-grants?status=active").await;
        assert_eq!(active["page"]["total"], 2);
        let (_, revoked) = get(router, "/api/providers/target-grants?status=revoked").await;
        assert_eq!(revoked["page"]["total"], 1);
        assert_eq!(revoked["items"][0]["grantId"], preview_grant);
        let (_, by_target) = get(
            router,
            &format!(
                "/api/providers/target-grants?target={}&status=active",
                seeded.target_id
            ),
        )
        .await;
        assert_eq!(by_target["page"]["total"], 1);
        assert_eq!(by_target["items"][0]["grantId"], seeded.grant_id);
        let (_, other_target) = get(
            router,
            &format!("/api/providers/target-grants?target={second_target}"),
        )
        .await;
        assert_eq!(other_target["page"]["total"], 1);

        for uri in [
            "/api/providers/target-grants?status=bogus",
            "/api/providers/target-grants?target=not-an-id",
            "/api/providers/target-grants?unknown=1",
        ] {
            let (status, _) = get(router, uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }
        let (status, _) = get(
            router,
            &format!("/api/providers/target-grants/{}", CanonicalId::new_v7()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A driver that answers every step with a successful receipt, pinned
    /// to the fixture's manifest so the target's durable pin accepts it.
    struct HttpTestDriver {
        manifest: ValidatedProviderDriverManifest,
        calls: std::sync::Mutex<Vec<deis_provider_driver::DriverOperation>>,
    }

    #[async_trait::async_trait]
    impl deis_provider_driver::ProviderDriver for HttpTestDriver {
        fn manifest(&self) -> &ValidatedProviderDriverManifest {
            &self.manifest
        }

        async fn execute(
            &self,
            context: &deis_provider_driver::DriverExecutionContext<'_>,
        ) -> Result<deis_provider_driver::DriverReceipt, deis_provider_driver::DriverError>
        {
            use deis_provider_driver::{
                DriverNextAction, DriverOperation, DriverStepState, SideEffect,
            };
            let request = context.request();
            self.calls.lock().unwrap().push(request.operation);
            let side_effecting = matches!(
                request.operation,
                DriverOperation::Upload
                    | DriverOperation::Activate
                    | DriverOperation::Rollback
                    | DriverOperation::Cleanup
            );
            Ok(deis_provider_driver::DriverReceipt {
                schema_version: 1,
                operation_id: request.operation_id,
                operation: request.operation,
                desired_state_digest: request.desired_state_digest.clone(),
                state: DriverStepState::Succeeded,
                deployment_state: None,
                side_effect: if side_effecting {
                    SideEffect::Confirmed
                } else {
                    SideEffect::None
                },
                provider_request_id: Some(format!("request-{}", request.operation.as_str())),
                provider_version_id: (request.operation == DriverOperation::Upload)
                    .then(|| "provider-version-1".to_owned()),
                provider_deployment_id: matches!(
                    request.operation,
                    DriverOperation::Activate
                        | DriverOperation::Observe
                        | DriverOperation::Rollback
                )
                .then(|| "provider-deployment-1".to_owned()),
                provider_resource_ids: Default::default(),
                observed_at: chrono::Utc::now(),
                next_action: DriverNextAction::None,
                diagnostic: None,
            })
        }
    }

    async fn drive_once(seeded: &ProviderFixture, driver: &HttpTestDriver, operation_id: &str) {
        seeded
            .harness
            .state
            .store
            .drive_provider_deployment_once_for_test(
                operation_id.parse().unwrap(),
                CanonicalId::new_v7(),
                driver,
                deis_provider_driver::DriverOrchestratorConfig::default(),
            )
            .await
            .unwrap();
    }

    fn error_code(response: &axum::response::Response) -> &'static str {
        response
            .extensions()
            .get::<crate::api::error::ErrorDetail>()
            .cloned()
            .unwrap()
            .code
    }

    #[tokio::test]
    async fn provider_deployment_cancel_is_revision_fenced_replayable_and_settled_by_the_dispatcher()
     {
        let fixture = ReleaseFixture::new();
        let seeded = provider_fixture(&fixture).await;
        let router = &seeded.router;
        let artifact = attach_artifact(&seeded.harness).await;
        let (status, operation) =
            begin_deployment(router, &seeded.target_id, &artifact, "deploy-to-cancel").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{operation}");
        assert_eq!(operation["cancellable"], true);
        let operation_id = operation["operationId"].as_str().unwrap().to_owned();
        // Recording the first step already moved the operation past its
        // initial revision; the ETag the caller saw is what If-Match names.
        let revision = operation["revision"].as_u64().unwrap();
        let etag = |revision: u64| format!("\"{revision}\"");
        let deployment_uri =
            format!("/api/apps/demo/environments/production/provider-deployments/{operation_id}");
        let cancel_uri = format!("{deployment_uri}/cancel");

        // If-Match is mandatory, exactly as for the generic operation cancel.
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&cancel_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
        let (status, _, stale) = mutate(
            router,
            Method::POST,
            &cancel_uri,
            "cancel-001",
            &etag(revision + 5),
        )
        .await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{stale}");
        // Another environment never sees the operation.
        let (status, _, elsewhere) = mutate(
            router,
            Method::POST,
            &format!(
                "/api/apps/demo/environments/preview/provider-deployments/{operation_id}/cancel"
            ),
            "cancel-elsewhere",
            &etag(revision),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{elsewhere}");

        let (status, headers, cancelling) = mutate(
            router,
            Method::POST,
            &cancel_uri,
            "cancel-001",
            &etag(revision),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{cancelling}");
        assert_eq!(headers[header::LOCATION], deployment_uri);
        assert_eq!(headers[header::ETAG], etag(revision + 1));
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(cancelling["operationId"], operation_id);
        assert_eq!(cancelling["state"], "cancelling");
        assert_eq!(cancelling["cancellable"], false);

        // The same key replays the same operation, stale If-Match and all;
        // a different key finds nothing cancellable.
        let (status, headers, replayed) = mutate(
            router,
            Method::POST,
            &cancel_uri,
            "cancel-001",
            &etag(revision),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{replayed}");
        assert_eq!(headers[header::ETAG], etag(revision + 1));
        assert_eq!(replayed, cancelling);
        let response = router
            .clone()
            .oneshot(keyed_request(
                Method::POST,
                &cancel_uri,
                "cancel-002",
                &etag(revision + 1),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            error_code(&response),
            crate::api::error::codes::OPERATION_NOT_CANCELLABLE
        );

        // The dispatcher settles it: nothing ran, nothing to clean up.
        assert_eq!(
            ready_for_dispatch(&seeded.harness).await,
            std::slice::from_ref(&operation_id)
        );
        let driver = HttpTestDriver {
            manifest: fixture.manifest.clone(),
            calls: std::sync::Mutex::new(Vec::new()),
        };
        drive_once(&seeded, &driver, &operation_id).await;
        assert!(driver.calls.lock().unwrap().is_empty());
        assert!(ready_for_dispatch(&seeded.harness).await.is_empty());
        let (status, workflow) = get(router, &deployment_uri).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(workflow["kind"], "deploy");
        assert_eq!(workflow["operation"]["state"], "cancelled");
        assert_eq!(workflow["operation"]["cancellable"], false);
        assert!(workflow["cancelRequestedAt"].is_string());
        assert!(workflow["rolledBackFromOperationId"].is_null());
        assert_eq!(workflow["steps"].as_array().unwrap().len(), 1);
        // A terminal operation refuses a fresh cancel with the stable code.
        let response = router
            .clone()
            .oneshot(keyed_request(
                Method::POST,
                &cancel_uri,
                "cancel-003",
                &etag(workflow["operation"]["revision"].as_u64().unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            error_code(&response),
            crate::api::error::codes::OPERATION_NOT_CANCELLABLE
        );
        // And the replay still answers the original cancel.
        let (status, _, replayed) = mutate(
            router,
            Method::POST,
            &cancel_uri,
            "cancel-001",
            &etag(revision),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{replayed}");
        assert_eq!(replayed["state"], "cancelled");
    }

    #[tokio::test]
    async fn provider_deployment_rollback_creates_a_linked_operation_that_runs_the_driver_rollback()
    {
        let fixture = ReleaseFixture::new();
        let seeded = provider_fixture(&fixture).await;
        let router = &seeded.router;
        let artifact = attach_artifact(&seeded.harness).await;
        let (status, operation) =
            begin_deployment(router, &seeded.target_id, &artifact, "deploy-to-roll-back").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{operation}");
        let deployment_id = operation["operationId"].as_str().unwrap().to_owned();
        let deployment_uri =
            format!("/api/apps/demo/environments/production/provider-deployments/{deployment_id}");
        let rollback_uri = format!("{deployment_uri}/rollback");
        let driver = HttpTestDriver {
            manifest: fixture.manifest.clone(),
            calls: std::sync::Mutex::new(Vec::new()),
        };

        // Only a succeeded deployment can be rolled back to.
        let response = router
            .clone()
            .oneshot(keyed_request(
                Method::POST,
                &rollback_uri,
                "rollback-early",
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            error_code(&response),
            crate::api::error::codes::OPERATION_NOT_ROLLBACKABLE
        );
        for _ in 0..crate::catalog::PROVIDER_DEPLOYMENT_STEPS.len() {
            drive_once(&seeded, &driver, &deployment_id).await;
        }
        let (status, succeeded) = get(router, &deployment_uri).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(succeeded["operation"]["state"], "succeeded");
        assert_eq!(succeeded["rollbackOperationIds"], json!([]));

        // The key is required: a rollback creates work.
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&rollback_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // Another environment never sees the deployment.
        let (status, _, elsewhere) = mutate(
            router,
            Method::POST,
            &format!(
                "/api/apps/demo/environments/preview/provider-deployments/{deployment_id}/rollback"
            ),
            "rollback-elsewhere",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{elsewhere}");

        let (status, headers, rollback) =
            mutate(router, Method::POST, &rollback_uri, "rollback-001", "").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{rollback}");
        let rollback_id = rollback["operationId"].as_str().unwrap().to_owned();
        assert_ne!(rollback_id, deployment_id);
        assert_eq!(rollback["kind"], "rollback");
        assert_eq!(rollback["state"], "queued");
        assert_eq!(rollback["phase"], "rollback");
        let rollback_resource =
            format!("/api/apps/demo/environments/production/provider-deployments/{rollback_id}");
        assert_eq!(headers[header::LOCATION], rollback_resource);
        assert_eq!(
            headers[header::ETAG],
            format!("\"{}\"", rollback["revision"].as_u64().unwrap())
        );
        // Replays answer the same operation; the link is visible both ways
        // and in the list.
        let (status, _, replayed) =
            mutate(router, Method::POST, &rollback_uri, "rollback-001", "").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{replayed}");
        assert_eq!(replayed["operationId"], rollback_id);
        let (status, workflow) = get(router, &rollback_resource).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(workflow["kind"], "rollback");
        assert_eq!(workflow["rolledBackFromOperationId"], deployment_id);
        assert_eq!(
            workflow["desiredStateDigest"],
            succeeded["desiredStateDigest"]
        );
        assert_eq!(workflow["artifact"], succeeded["artifact"]);
        assert_eq!(workflow["steps"][0]["operation"], "rollback");
        let (_, source) = get(router, &deployment_uri).await;
        assert_eq!(source["rollbackOperationIds"], json!([rollback_id]));
        let (status, listed) = get(
            router,
            "/api/apps/demo/environments/production/provider-deployments",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed["page"]["total"], 2);
        assert_eq!(listed["items"][0]["operation"]["operationId"], rollback_id);
        assert_eq!(
            listed["items"][1]["operation"]["operationId"],
            deployment_id
        );
        // A rollback is not itself a deployment to roll back to.
        let response = router
            .clone()
            .oneshot(keyed_request(
                Method::POST,
                &format!("{rollback_resource}/rollback"),
                "rollback-nested",
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            error_code(&response),
            crate::api::error::codes::OPERATION_NOT_ROLLBACKABLE
        );

        // The dispatcher runs the driver's rollback against the deployment's
        // desired state, then observes.
        for _ in 0..crate::catalog::PROVIDER_ROLLBACK_STEPS.len() {
            drive_once(&seeded, &driver, &rollback_id).await;
        }
        let calls = driver.calls.lock().unwrap().clone();
        assert_eq!(
            &calls[crate::catalog::PROVIDER_DEPLOYMENT_STEPS.len()..],
            [
                deis_provider_driver::DriverOperation::Rollback,
                deis_provider_driver::DriverOperation::Observe,
            ]
        );
        let (_, done) = get(router, &rollback_resource).await;
        assert_eq!(done["operation"]["state"], "succeeded");
        assert_eq!(done["operation"]["progress"], 10000);
        assert_eq!(done["nextStepIndex"], 2);
        let rollback_child: CanonicalId = done["steps"][0]["childOperationId"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let record = seeded
            .harness
            .state
            .store
            .provider_driver_operation(rollback_child)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.desired_state_digest.as_str(),
            succeeded["desiredStateDigest"].as_str().unwrap()
        );
        assert_eq!(
            record.receipt.unwrap().provider_deployment_id.as_deref(),
            Some("provider-deployment-1")
        );
    }
}
