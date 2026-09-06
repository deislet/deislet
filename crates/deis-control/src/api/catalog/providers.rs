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

use super::{CatalogState, actor_subject};
use crate::{
    api::{
        error::{ApiError, ApiResult},
        pagination::{ControlListPage, ListQuery, list_page},
    },
    catalog::{
        IdempotencyOutcome, IdempotencyScope, IdempotencyStore, IdempotentRequest,
        IdempotentResource, OperationRequestContext, PagePosition, PendingIdempotencyRecord,
        ProviderCredentialAuditEvent, ProviderCredentialBinding, ProviderCredentialBindingSpec,
        ProviderCredentialMutationContext, ProviderDeploymentSpec, ProviderDeploymentWorkflow,
        ProviderDriverReleaseAuditEvent, ProviderDriverReleaseRecord,
        ProviderDriverReleaseRegistry, ProviderInstallation, ProviderInstallationSpec,
        ProviderLifecycleAuditEvent, ProviderLifecycleMutationContext,
        ProviderLifecycleResourceKind, ProviderTarget, ProviderTargetGrant,
        ProviderTargetGrantAuditEvent, ProviderTargetGrantHistoryFilter,
        ProviderTargetGrantMutationContext, ProviderTargetGrantSpec,
        ProviderTargetGrantStatusFilter, ProviderTargetSpec,
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProvisionCredentialRequest {
    pub backend_id: String,
    pub name: String,
    pub credential: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RotateCredentialRequest {
    pub credential: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisterInstallationRequest {
    pub backend_id: String,
    pub account_reference: String,
    pub region: Option<String>,
    pub credential_binding_id: CanonicalId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisterTargetRequest {
    pub name: String,
    pub standard_version: String,
    pub target_config_digest: Sha256Digest,
    pub driver_manifest: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BeginProviderDeploymentRequest {
    pub provider_target_id: CanonicalId,
    pub artifact: ArtifactVersionRef,
    #[serde(default)]
    pub plan: ProviderNeutralPlan,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantProviderTargetRequest {
    pub provider_target_id: CanonicalId,
    pub note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderTargetGrantListQuery {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderTargetGrantHistoryQuery {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub status: Option<String>,
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteInstallationQuery {
    pub cascade: Option<bool>,
}

pub async fn install_driver_release(
    State(state): State<CatalogState>,
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
        &format!(
            "/api/providers/driver-releases/{}",
            installed.release_digest
        ),
        installed.revision,
    )?;
    Ok((status, headers, Json(installed)))
}

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

pub async fn provision_credential(
    State(state): State<CatalogState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Json(request): Json<ProvisionCredentialRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderCredentialBinding>)> {
    let idempotency_key = required_header(&headers, IDEMPOTENCY_KEY)?;
    let replay = idempotent_request(
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
            format!("/api/providers/credentials/{}", binding.binding_id)
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

pub async fn revoke_credential(
    State(state): State<CatalogState>,
    Path(binding_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderCredentialBinding>)> {
    let binding_id = parse_id(&binding_id, "provider credential binding")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
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

pub async fn register_installation(
    State(state): State<CatalogState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Json(request): Json<RegisterInstallationRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderInstallation>)> {
    let idempotency_key = required_header(&headers, IDEMPOTENCY_KEY)?;
    let replay = idempotent_request(
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
            format!(
                "/api/providers/installations/{}",
                installation.installation_id
            )
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

pub async fn register_target(
    State(state): State<CatalogState>,
    Path(installation_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Json(request): Json<RegisterTargetRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderTarget>)> {
    let idempotency_key = required_header(&headers, IDEMPOTENCY_KEY)?;
    let installation_id = parse_id(&installation_id, "provider installation")?;
    let replay = idempotent_request(
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
        |target: &ProviderTarget| format!("/api/providers/targets/{}", target.target_id),
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

pub async fn disable_installation(
    State(state): State<CatalogState>,
    Path(installation_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderInstallation>)> {
    let installation_id = parse_id(&installation_id, "provider installation")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
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

pub async fn enable_installation(
    State(state): State<CatalogState>,
    Path(installation_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderInstallation>)> {
    let installation_id = parse_id(&installation_id, "provider installation")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
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

pub async fn disable_target(
    State(state): State<CatalogState>,
    Path(target_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderTarget>)> {
    let target_id = parse_id(&target_id, "provider target")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
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

pub async fn enable_target(
    State(state): State<CatalogState>,
    Path(target_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderTarget>)> {
    let target_id = parse_id(&target_id, "provider target")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
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
pub async fn delete_target(
    State(state): State<CatalogState>,
    Path(target_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderTarget>)> {
    let target_id = parse_id(&target_id, "provider target")?;
    let expected_revision = required_if_match(&headers)?;
    let replay = idempotent_request(
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

pub async fn grant_provider_target(
    State(state): State<CatalogState>,
    Path((app_name, environment_name)): Path<(String, String)>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
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
            format!(
                "/api/apps/{app_name}/environments/{environment_name}/provider-target-grants/{}",
                grant.grant_id
            )
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

pub async fn begin_provider_deployment(
    State(state): State<CatalogState>,
    Path((app_name, environment_name)): Path<(String, String)>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Json(request): Json<BeginProviderDeploymentRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProductOperation>)> {
    let app = state.store.get_app(&app_name).await?;
    let environment = state
        .store
        .get_environment(&app_name, &environment_name)
        .await?;
    let context = operation_context(&headers, &principal, &app_name, &environment_name)?;
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
    let headers = operation_headers(&operation, &app_name, &environment_name)?;
    Ok((StatusCode::ACCEPTED, headers, Json(operation)))
}

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

pub async fn get_provider_deployment(
    State(state): State<CatalogState>,
    Path((app_name, environment_name, operation_id)): Path<(String, String, String)>,
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
    let headers = operation_headers(&workflow.operation, &app_name, &environment_name)?;
    Ok((headers, Json(workflow)))
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
fn idempotent_request(
    route_id: &'static str,
    idempotency_key: Option<&str>,
    canonical_request: serde_json::Value,
) -> ApiResult<Option<IdempotentRequest>> {
    idempotency_key
        .map(|idempotency_key| {
            let canonical = serde_jcs::to_vec(&canonical_request).map_err(|error| {
                ApiError::InternalError(format!(
                    "cannot canonicalize {route_id} request for its idempotency record: {error}"
                ))
            })?;
            IdempotentRequest::new(route_id, idempotency_key, &canonical).map_err(Into::into)
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

fn operation_context(
    headers: &HeaderMap,
    principal: &Principal,
    app_name: &str,
    environment_name: &str,
) -> ApiResult<OperationRequestContext> {
    let idempotency_key = required_header(headers, IDEMPOTENCY_KEY)?;
    let request_id =
        optional_canonical_header(headers, REQUEST_ID)?.unwrap_or_else(CanonicalId::new_v7);
    let correlation_id = optional_canonical_header(headers, CORRELATION_ID)?.unwrap_or(request_id);
    OperationRequestContext::new(
        idempotency_key,
        &format!(
            "POST\n/api/apps/{app_name}/environments/{environment_name}/provider-deployments\nv1"
        ),
        actor_subject(principal),
        request_id,
        correlation_id,
    )
    .map_err(Into::into)
}

fn operation_headers(
    operation: &ProductOperation,
    app_name: &str,
    environment_name: &str,
) -> ApiResult<HeaderMap> {
    let mut headers = resource_headers(
        &format!(
            "/api/apps/{app_name}/environments/{environment_name}/provider-deployments/{}",
            operation.operation_id
        ),
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

fn required_header<'a>(headers: &'a HeaderMap, name: &str) -> ApiResult<&'a str> {
    single_header(headers, name)?
        .ok_or_else(|| ApiError::BadRequest(format!("{name} header is required")))?
        .to_str()
        .map_err(|_| ApiError::BadRequest(format!("{name} header is not valid visible ASCII")))
}

fn optional_header<'a>(headers: &'a HeaderMap, name: &str) -> ApiResult<Option<&'a str>> {
    single_header(headers, name)?
        .map(|value| {
            value.to_str().map_err(|_| {
                ApiError::BadRequest(format!("{name} header is not valid visible ASCII"))
            })
        })
        .transpose()
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

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> ApiResult<Option<&'a HeaderValue>> {
    let all = headers.get_all(name);
    let mut values = all.iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(ApiError::BadRequest(format!(
            "{name} header must appear exactly once"
        )));
    }
    Ok(first)
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
        crate::api::catalog::router(state.clone()).layer(Extension(Principal {
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
        let router = crate::api::catalog::router(state).layer(Extension(Principal::anonymous()));
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
}
