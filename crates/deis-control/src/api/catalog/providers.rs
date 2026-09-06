//! Provider-neutral management API.
//!
//! Credential values enter only the vault write handlers. They are never
//! returned, formatted with `Debug`, copied into an operation, or inspected
//! by the authorization middleware. Every opaque provider resource lives on
//! the installation-wide administrator surface. Deployment status is scoped
//! by the application and environment names in its path; starting one stays
//! administrative until the product model can grant a target to that scope.

use axum::{
    Json,
    extract::{Extension, Multipart, Path, Query, State, multipart::Field},
    http::{HeaderMap, HeaderValue, StatusCode, header},
};
use deis_provider_driver::{
    ArtifactVersionRef, CredentialMaterial, ProviderNeutralPlan, Sha256Digest,
    ValidatedDriverReleaseAuthorization, ValidatedDriverReleaseTransparencyProof,
    ValidatedProviderDriverManifest, ValidatedProviderDriverRelease,
};
use deis_wire::{CanonicalId, ProductOperation};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::CatalogState;
use crate::{
    api::error::{ApiError, ApiResult},
    catalog::{
        OperationRequestContext, ProviderCredentialAuditEvent, ProviderCredentialBinding,
        ProviderCredentialBindingSpec, ProviderCredentialMutationContext, ProviderDeploymentSpec,
        ProviderDeploymentWorkflow, ProviderDriverReleaseAuditEvent, ProviderDriverReleaseRecord,
        ProviderDriverReleaseRegistry, ProviderInstallation, ProviderInstallationSpec,
        ProviderTarget, ProviderTargetSpec,
    },
    identity::{Principal, PrincipalKind},
};

const IDEMPOTENCY_KEY: &str = "idempotency-key";
const REQUEST_ID: &str = "x-request-id";
const CORRELATION_ID: &str = "x-correlation-id";
const IF_MATCH: &str = "if-match";
const DEFAULT_LIST_LIMIT: usize = 100;
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
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub limit: Option<usize>,
}

impl ListQuery {
    fn bounded_limit(&self) -> ApiResult<usize> {
        let limit = self.limit.unwrap_or(DEFAULT_LIST_LIMIT);
        if !(1..=1_000).contains(&limit) {
            return Err(ApiError::BadRequest(
                "limit must be between 1 and 1000".to_string(),
            ));
        }
        Ok(limit)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialBindingsResponse {
    pub bindings: Vec<ProviderCredentialBinding>,
    pub total: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialAuditResponse {
    pub events: Vec<ProviderCredentialAuditEvent>,
    pub total: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstallationsResponse {
    pub installations: Vec<ProviderInstallation>,
    pub total: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTargetsResponse {
    pub targets: Vec<ProviderTarget>,
    pub total: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDeploymentsResponse {
    pub deployments: Vec<ProviderDeploymentWorkflow>,
    pub total: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDriverReleasesResponse {
    pub releases: Vec<ProviderDriverReleaseRecord>,
    pub total: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDriverReleaseAuditResponse {
    pub events: Vec<ProviderDriverReleaseAuditEvent>,
    pub total: usize,
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
) -> ApiResult<(HeaderMap, Json<ProviderDriverReleasesResponse>)> {
    let releases = provider_registry(&state)?
        .releases(query.bounded_limit()?)
        .await?;
    let total = releases.len();
    Ok((
        no_store_headers(),
        Json(ProviderDriverReleasesResponse { releases, total }),
    ))
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
) -> ApiResult<(HeaderMap, Json<ProviderDriverReleaseAuditResponse>)> {
    let release_digest = parse_digest(&release_digest, "provider driver release")?;
    let registry = provider_registry(&state)?;
    if registry.release(release_digest.clone()).await?.is_none() {
        return Err(ApiError::NotFound(format!(
            "provider driver release {release_digest} not found"
        )));
    }
    let events = registry
        .audit_events(release_digest, query.bounded_limit()?)
        .await?;
    let total = events.len();
    Ok((
        no_store_headers(),
        Json(ProviderDriverReleaseAuditResponse { events, total }),
    ))
}

pub async fn provision_credential(
    State(state): State<CatalogState>,
    Extension(principal): Extension<Principal>,
    Json(request): Json<ProvisionCredentialRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderCredentialBinding>)> {
    let material = credential_material(request.credential)?;
    let binding = state
        .provider_credentials
        .provision(
            ProviderCredentialBindingSpec {
                backend_id: request.backend_id,
                name: request.name,
            },
            material,
            credential_mutation_context(&principal),
        )
        .await?;
    let headers = resource_headers(
        &format!("/api/providers/credentials/{}", binding.binding_id),
        binding.row_revision,
    )?;
    Ok((StatusCode::CREATED, headers, Json(binding)))
}

pub async fn list_credentials(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(HeaderMap, Json<ProviderCredentialBindingsResponse>)> {
    let bindings = state
        .provider_credentials
        .bindings(query.bounded_limit()?)
        .await?;
    let total = bindings.len();
    Ok((
        no_store_headers(),
        Json(ProviderCredentialBindingsResponse { bindings, total }),
    ))
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
) -> ApiResult<(HeaderMap, Json<ProviderCredentialBinding>)> {
    let binding_id = parse_id(&binding_id, "provider credential binding")?;
    let expected_revision = required_if_match(&headers)?;
    let material = credential_material(request.credential)?;
    let binding = state
        .provider_credentials
        .rotate(
            binding_id,
            expected_revision,
            material,
            credential_mutation_context(&principal),
        )
        .await?;
    let headers = resource_headers("", binding.row_revision)?;
    Ok((headers, Json(binding)))
}

pub async fn revoke_credential(
    State(state): State<CatalogState>,
    Path(binding_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> ApiResult<(HeaderMap, Json<ProviderCredentialBinding>)> {
    let binding_id = parse_id(&binding_id, "provider credential binding")?;
    let expected_revision = required_if_match(&headers)?;
    let binding = state
        .provider_credentials
        .revoke(
            binding_id,
            expected_revision,
            credential_mutation_context(&principal),
        )
        .await?;
    let headers = resource_headers("", binding.row_revision)?;
    Ok((headers, Json(binding)))
}

pub async fn credential_audit(
    State(state): State<CatalogState>,
    Path(binding_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(HeaderMap, Json<ProviderCredentialAuditResponse>)> {
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
    let events = state
        .provider_credentials
        .audit_events(binding_id, query.bounded_limit()?)
        .await?;
    let total = events.len();
    Ok((
        no_store_headers(),
        Json(ProviderCredentialAuditResponse { events, total }),
    ))
}

pub async fn register_installation(
    State(state): State<CatalogState>,
    Json(request): Json<RegisterInstallationRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderInstallation>)> {
    let installation = state
        .store
        .register_provider_installation(ProviderInstallationSpec {
            backend_id: request.backend_id,
            account_reference: request.account_reference,
            region: request.region,
            credential_binding_id: request.credential_binding_id,
        })
        .await?;
    let headers = resource_headers(
        &format!(
            "/api/providers/installations/{}",
            installation.installation_id
        ),
        installation.revision,
    )?;
    Ok((StatusCode::CREATED, headers, Json(installation)))
}

pub async fn list_installations(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(HeaderMap, Json<ProviderInstallationsResponse>)> {
    let installations = state
        .store
        .provider_installations(query.bounded_limit()?)
        .await?;
    let total = installations.len();
    Ok((
        no_store_headers(),
        Json(ProviderInstallationsResponse {
            installations,
            total,
        }),
    ))
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
    Json(request): Json<RegisterTargetRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProviderTarget>)> {
    let installation_id = parse_id(&installation_id, "provider installation")?;
    let manifest_bytes = serde_json::to_vec(&request.driver_manifest).map_err(|error| {
        ApiError::BadRequest(format!(
            "provider driver manifest is not valid JSON: {error}"
        ))
    })?;
    let driver_manifest =
        ValidatedProviderDriverManifest::parse_json(&manifest_bytes).map_err(|error| {
            ApiError::BadRequest(format!("invalid provider driver manifest: {error}"))
        })?;
    let target = state
        .store
        .register_provider_target(ProviderTargetSpec {
            installation_id,
            name: request.name,
            standard_version: request.standard_version,
            target_config_digest: request.target_config_digest,
            driver_manifest,
        })
        .await?;
    let headers = resource_headers(
        &format!("/api/providers/targets/{}", target.target_id),
        target.revision,
    )?;
    Ok((StatusCode::CREATED, headers, Json(target)))
}

pub async fn list_installation_targets(
    State(state): State<CatalogState>,
    Path(installation_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(HeaderMap, Json<ProviderTargetsResponse>)> {
    let installation_id = parse_id(&installation_id, "provider installation")?;
    let targets = state
        .store
        .provider_targets(Some(installation_id), query.bounded_limit()?)
        .await?;
    let total = targets.len();
    Ok((
        no_store_headers(),
        Json(ProviderTargetsResponse { targets, total }),
    ))
}

pub async fn list_targets(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<(HeaderMap, Json<ProviderTargetsResponse>)> {
    let targets = state
        .store
        .provider_targets(None, query.bounded_limit()?)
        .await?;
    let total = targets.len();
    Ok((
        no_store_headers(),
        Json(ProviderTargetsResponse { targets, total }),
    ))
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
) -> ApiResult<(HeaderMap, Json<ProviderDeploymentsResponse>)> {
    let app = state.store.get_app(&app_name).await?;
    let environment = state
        .store
        .get_environment(&app_name, &environment_name)
        .await?;
    let deployments = state
        .store
        .provider_deployments_for_environment(
            app.canonical_id,
            environment.canonical_id,
            query.bounded_limit()?,
        )
        .await?;
    let total = deployments.len();
    Ok((
        no_store_headers(),
        Json(ProviderDeploymentsResponse { deployments, total }),
    ))
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

fn credential_mutation_context(principal: &Principal) -> ProviderCredentialMutationContext {
    ProviderCredentialMutationContext {
        actor_subject: actor_subject(principal),
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

fn actor_subject(principal: &Principal) -> String {
    match principal.kind {
        PrincipalKind::Account => format!(
            "account:{}",
            principal.account.as_deref().unwrap_or("unknown")
        ),
        PrincipalKind::ConfigKey => format!(
            "config-key:{}",
            principal.credential_id.as_deref().unwrap_or("unknown")
        ),
        PrincipalKind::Anonymous => "anonymous:direct-local".to_string(),
    }
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
                "If-Match with the current provider credential ETag is required".to_string(),
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

    fn json_request(method: Method, uri: &str, value: Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap()
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
        let router = crate::api::catalog::router(state);
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
        assert_eq!(listed["total"], 1);
        assert_eq!(listed["releases"][0]["releaseDigest"], release_digest);

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
        assert_eq!(audit["total"], 1);
        assert_eq!(audit["events"][0]["eventType"], "registered");

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
        assert_eq!(audit["total"], 2);
        assert_eq!(audit["events"][0]["eventType"], "registered");
        assert_eq!(audit["events"][1]["eventType"], "verified");
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
        assert_eq!(audit["total"], 2);
        assert_eq!(audit["events"][0]["eventType"], "provisioned");
        assert_eq!(audit["events"][1]["eventType"], "rotated");
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
        assert_eq!(targets["total"], 0);
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
            .oneshot(json_request(
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
        let mut request = json_request(
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
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            response_json(response).await["error"]
                .as_str()
                .unwrap()
                .contains(&missing_target.to_string())
        );

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
        assert_eq!(deployments["total"], 0);
    }
}
