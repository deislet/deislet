//! Deployment endpoints.
//!
//! A deployment is a pointer row, so every operation here is an insert:
//! deploying moves an environment forward, rolling back moves it
//! backwards, and both leave the full history behind them.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::LOCATION},
};
use deis_wire::CanonicalId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tracing::{error, info, warn};
use utoipa::ToSchema;

use super::CatalogState;
use super::versions::VersionView;
use crate::api::error::{ApiError, ApiResult};
use crate::api::openapi;
use crate::api::pagination::{ControlListPage, ListQuery, environment_scope, list_page};
use crate::api::paths::ApiPaths;
use crate::catalog::{
    CatalogError, CatalogStore, Deployment, DeploymentDrainObservation, DeploymentEvent,
    DeploymentGenerationDrain, DeploymentPlan, DeploymentPlanSpec, DeploymentProxyObservation,
    DeploymentQueueObservation, DeploymentTargetObservation, Environment, PagePosition, Version,
    now_secs,
};
use crate::delivery::{DeliveryReason, DeliveryReport, DeploymentPayload};
use crate::message::NodeRole;

const SECRET_REVISION_GC_INTERVAL_SECONDS: i64 = 300;
const DEPLOYMENT_COORDINATOR_LEASE_SECONDS: u64 = 30;
const DEPLOYMENT_COORDINATOR_HEARTBEAT_SECONDS: u64 = 10;

/// One coordination attempt owns one distinct durable lease identity. Keeping
/// this separate from a process identity also excludes the request handler and
/// background reconciler inside the same Control instance.
struct DeploymentPlanLease {
    store: Arc<CatalogStore>,
    plan_id: String,
    coordinator_id: CanonicalId,
    lost: Arc<AtomicBool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    heartbeat: tokio::task::JoinHandle<()>,
}

impl DeploymentPlanLease {
    fn start(store: Arc<CatalogStore>, plan_id: String, coordinator_id: CanonicalId) -> Self {
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let heartbeat_store = Arc::clone(&store);
        let heartbeat_plan = plan_id.clone();
        let lost = Arc::new(AtomicBool::new(false));
        let heartbeat_lost = Arc::clone(&lost);
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = tokio::time::sleep(Duration::from_secs(
                        DEPLOYMENT_COORDINATOR_HEARTBEAT_SECONDS,
                    )) => {
                        if let Err(error) = heartbeat_store
                            .renew_deployment_plan_lease(
                                &heartbeat_plan,
                                coordinator_id,
                                DEPLOYMENT_COORDINATOR_LEASE_SECONDS,
                            )
                            .await
                        {
                            heartbeat_lost.store(true, Ordering::Release);
                            warn!(
                                deployment_plan = %heartbeat_plan,
                                %coordinator_id,
                                %error,
                                "Deployment coordinator lost its durable lease; no next phase action may be sent"
                            );
                            break;
                        }
                    }
                }
            }
        });
        Self {
            store,
            plan_id,
            coordinator_id,
            lost,
            stop: Some(stop),
            heartbeat,
        }
    }

    fn coordinator_id(&self) -> CanonicalId {
        self.coordinator_id
    }

    /// Fence every external action with a synchronous renewal. The heartbeat
    /// keeps long RPCs alive; this check prevents an attempt that already lost
    /// ownership from sending the next idempotent phase command.
    async fn ensure(&self) -> ApiResult<()> {
        if self.lost.load(Ordering::Acquire) {
            return Err(ApiError::Conflict(format!(
                "deployment plan {} coordinator lease was lost",
                self.plan_id
            )));
        }
        if let Err(error) = self
            .store
            .renew_deployment_plan_lease(
                &self.plan_id,
                self.coordinator_id,
                DEPLOYMENT_COORDINATOR_LEASE_SECONDS,
            )
            .await
        {
            self.lost.store(true, Ordering::Release);
            return Err(error.into());
        }
        Ok(())
    }

    async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = self.heartbeat.await;
        if let Err(error) = self
            .store
            .release_deployment_plan_lease(&self.plan_id, self.coordinator_id)
            .await
        {
            // A newer owner is never cleared: release is fenced by the exact
            // owner identity. The message is still useful if shutdown or an
            // expired pass handed work to another reconciler meanwhile.
            warn!(
                deployment_plan = %self.plan_id,
                coordinator_id = %self.coordinator_id,
                %error,
                "Deployment coordinator lease release did not apply"
            );
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DeployRequest {
    /// Target version by id. Takes precedence over `version_number`.
    #[serde(default)]
    pub version_id: Option<String>,
    /// Target version by its per-app number, i.e. the "v3" a human says.
    #[serde(default)]
    pub version_number: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
    /// Deployment-time config only. Secret values use the dedicated secret
    /// write API and are selected by immutable revision.
    #[serde(default)]
    pub config_values: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct RollbackRequest {
    /// Explicit target. When omitted, the previous distinct version this
    /// environment ran is used.
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub version_number: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeploymentResponse {
    /// Absent while an accepted plan is still draining its pre-commit queue
    /// barrier. No production pointer exists for that plan yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub deployment: Option<Deployment>,
    pub environment: String,
    pub version: VersionView,
    /// Which nodes took this version. Absent when the response only reads
    /// back an existing pointer, because then nothing was pushed.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub delivery: Option<DeliveryReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub plan: Option<DeploymentPlan>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeploymentPlanResponse {
    pub plan: DeploymentPlan,
    pub targets: Vec<DeploymentTargetObservation>,
    pub proxies: Vec<DeploymentProxyObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub queue: Option<DeploymentQueueObservation>,
    pub events: Vec<DeploymentEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub drain: Option<DeploymentGenerationDrain>,
    pub drain_observations: Vec<DeploymentDrainObservation>,
}

/// Deploy a version to an environment.
///
/// Build the exact payload, persist a plan and snapshot, prepare every frozen
/// runtime and trigger, then commit the pointer before activating them. An
/// environment whose secrets cannot be decrypted is refused before the plan
/// or pointer exists; a plan still draining its pre-commit barrier is returned
/// as `202 Accepted` and remains queryable through `Location`.
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/environments/{env_name}/deployments",
    operation_id = "deploy",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("env_name" = String, Path, description = "The environment's name.")),
    request_body = DeployRequest,
    responses(
        (status = 201, description = "The deployment was recorded and delivered.", body = DeploymentResponse),
        (status = 202, description = "The deployment was planned and is being coordinated; poll `Location`.", body = DeploymentResponse, headers(("Location" = String, description = "Where to poll the deployment plan."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn deploy(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
    headers: HeaderMap,
    paths: ApiPaths,
    Json(request): Json<DeployRequest>,
) -> ApiResult<(StatusCode, HeaderMap, Json<DeploymentResponse>)> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let version = resolve_version(
        &state,
        &app_id,
        request.version_id.as_deref(),
        request.version_number,
    )
    .await?;

    let base = state.deliverer.machine_base_url(&headers);
    let mut payload = state
        .deliverer
        .payload_with_config(&env, &version, &base, request.config_values)
        .await
        .inspect_err(|e| {
            warn!(
                app_id = %app_id,
                environment = %env_name,
                version_id = %version.id,
                error = %e,
                "Refused the deployment before recording it; the environment still runs the version it ran before"
            );
        })?;

    let (deployment, plan, delivery) = coordinate_strict_deployment(
        &state,
        &env,
        &version,
        &mut payload,
        request.note,
        DeliveryReason::Deployment,
    )
    .await?;
    let status = if plan.state == "active" {
        StatusCode::CREATED
    } else {
        StatusCode::ACCEPTED
    };

    Ok((
        status,
        deployment_plan_location(paths, &app_id, &env_name, &plan.id),
        Json(response(
            &env,
            deployment,
            &version,
            Some(delivery),
            Some(plan),
        )),
    ))
}

/// Roll an environment back to an earlier version.
///
/// Coordinate a new immutable deployment aimed at an older version and its
/// historical binding snapshot. The current pointer is changed only after
/// the same strict prepare barrier used by a forward deployment, so rollback
/// remains reversible and never silently rebinds secrets by name.
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/environments/{env_name}/rollback",
    operation_id = "rollback",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("env_name" = String, Path, description = "The environment's name.")),
    request_body(content = RollbackRequest, description = "Optional. Absent means the previous deployment."),
    responses(
        (status = 201, description = "The rollback was recorded and delivered.", body = DeploymentResponse),
        (status = 202, description = "The rollback was planned and is being coordinated; poll `Location`.", body = DeploymentResponse, headers(("Location" = String, description = "Where to poll the deployment plan."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn rollback(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
    headers: HeaderMap,
    paths: ApiPaths,
    body: Bytes,
) -> ApiResult<(StatusCode, HeaderMap, Json<DeploymentResponse>)> {
    let request = parse_rollback_body(&body).map_err(|detail| {
        warn!(
            app_id = %app_id,
            environment = %env_name,
            detail = %detail,
            "Rollback refused because its body did not parse; nothing moved, so the environment still runs the version it ran before"
        );
        ApiError::BadRequest(format!(
            "rollback body is not a valid rollback request: {detail}; send no body at all to roll back to the previous version"
        ))
    })?;
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;

    let version =
        if request.version_id.is_some() || request.version_number.is_some() {
            resolve_version(
                &state,
                &app_id,
                request.version_id.as_deref(),
                request.version_number,
            )
            .await?
        } else {
            let previous = state.store.previous_version(&env.id).await?.ok_or_else(|| {
            ApiError::Conflict(format!(
                "environment {env_name} of app {app_id} has no earlier version to roll back to"
            ))
        })?;
            state.store.get_version(&app_id, &previous).await?
        };

    let base = state.deliverer.machine_base_url(&headers);
    let historical_snapshot = state
        .store
        .deployment_snapshot_for_version(&env.id, &version.id)
        .await?;
    if historical_snapshot.is_none()
        && deis_bundle::Manifest::parse(&version.manifest)
            .map(|manifest| manifest.environment.is_some())
            .unwrap_or(false)
    {
        return Err(ApiError::Conflict(format!(
            "EC_ENV_ROLLBACK_SNAPSHOT_UNAVAILABLE: version {} has no historical environment snapshot to reactivate",
            version.id
        )));
    }
    let payload_result = if let Some((_, snapshot)) = historical_snapshot {
        state
            .deliverer
            .payload_for_snapshot(&env, &version, &base, snapshot)
            .await
    } else {
        state.deliverer.payload(&env, &version, &base).await
    };
    let mut payload = payload_result
        .inspect_err(|e| {
            warn!(
                app_id = %app_id,
                environment = %env_name,
                version_id = %version.id,
                error = %e,
                "Refused the rollback before recording it; the environment still runs the version it ran before"
            );
        })?;

    let note = request
        .note
        .unwrap_or_else(|| format!("rollback to version {}", version.number));
    let (deployment, plan, delivery) = coordinate_strict_deployment(
        &state,
        &env,
        &version,
        &mut payload,
        Some(note),
        DeliveryReason::Rollback,
    )
    .await?;

    info!(
        app_id = %app_id,
        environment = %env_name,
        version_id = %version.id,
        number = version.number,
        nodes = delivery.delivered.len(),
        "Rollback recorded as a new deployment; the previous pointer stays in the history"
    );
    Ok((
        if plan.state == "active" {
            StatusCode::CREATED
        } else {
            StatusCode::ACCEPTED
        },
        deployment_plan_location(paths, &app_id, &env_name, &plan.id),
        Json(response(
            &env,
            deployment,
            &version,
            Some(delivery),
            Some(plan),
        )),
    ))
}

/// The `Location` of the durable plan a mutation produced, spelled for the
/// surface the caller used.
pub(super) fn deployment_plan_location(
    paths: ApiPaths,
    app_id: &str,
    environment: &str,
    plan_id: &str,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let location = paths.absolute(&format!(
        "/apps/{app_id}/environments/{environment}/deployment-plans/{plan_id}"
    ));
    headers.insert(
        LOCATION,
        HeaderValue::from_str(&location)
            .expect("a canonical deployment plan id is a valid URI path"),
    );
    headers
}

/// Read what an environment is running.
///
/// The current version of an environment: the newest pointer row.
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/environments/{env_name}/deployment",
    operation_id = "getCurrentDeployment",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("env_name" = String, Path, description = "The environment's name.")),
    responses(
        (status = 200, description = "The deployment the environment points at.", body = DeploymentResponse),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn current_deployment(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
) -> ApiResult<Json<DeploymentResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let deployment = state
        .store
        .current_deployment(&env.id)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "environment {env_name} of app {app_id} has never been deployed"
            ))
        })?;
    let version = state
        .store
        .get_version(&app_id, &deployment.version_id)
        .await?;
    let plan = state.store.deployment_plan(&deployment.id).await.ok();
    Ok(Json(response(&env, Some(deployment), &version, None, plan)))
}

/// Read a deployment plan and what every node observed of it.
#[utoipa::path(
    get,
    path = "/api/v1/deployment-plans/{id}",
    operation_id = "getDeploymentPlan",
    tag = "product",
    params(("id" = String, Path, description = "The plan's id.")),
    responses(
        (status = 200, description = "The plan and what every node and proxy observed of it.", body = DeploymentPlanResponse),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn deployment_plan(
    State(state): State<CatalogState>,
    Path(id): Path<String>,
) -> ApiResult<Json<DeploymentPlanResponse>> {
    Ok(Json(deployment_plan_response(&state, &id).await?))
}

/// Scoped form used by deployment credentials. The path's environment is
/// resolved and compared with the plan, so a caller cannot put an authorised
/// prefix around another environment's opaque plan id.
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/environments/{env_name}/deployment-plans/{id}",
    operation_id = "getEnvironmentDeploymentPlan",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("env_name" = String, Path, description = "The environment's name."), ("id" = String, Path, description = "The plan's id.")),
    responses(
        (status = 200, description = "The plan and what every node and proxy observed of it.", body = DeploymentPlanResponse),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn deployment_plan_for_environment(
    State(state): State<CatalogState>,
    Path((app_id, env_name, id)): Path<(String, String, String)>,
) -> ApiResult<Json<DeploymentPlanResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let plan = state.store.deployment_plan(&id).await?;
    if plan.app_id != app_id || plan.env_id != env.id {
        return Err(ApiError::NotFound(format!(
            "deployment plan {id} in environment {env_name} of app {app_id}"
        )));
    }
    Ok(Json(deployment_plan_response(&state, &id).await?))
}

async fn deployment_plan_response(
    state: &CatalogState,
    id: &str,
) -> ApiResult<DeploymentPlanResponse> {
    let plan = state.store.deployment_plan(id).await?;
    let targets = state.store.deployment_target_observations(id).await?;
    let proxies = state.store.deployment_proxy_observations(id).await?;
    let queue = state.store.deployment_queue_observation(id).await?;
    let events = state.store.deployment_events(id).await?;
    let drain = state.store.deployment_generation_drain(id).await?;
    let drain_observations = state.store.deployment_drain_observations(id).await?;
    Ok(DeploymentPlanResponse {
        plan,
        targets,
        proxies,
        queue,
        events,
        drain,
        drain_observations,
    })
}

/// List an environment's deployment history, newest first.
///
/// The pointer history of one environment, newest first. Every row is a
/// deployment or a rollback; nothing is ever rewritten, so paging through
/// it is a stable walk backwards in time.
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/environments/{env_name}/deployments",
    operation_id = "listDeployments",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("env_name" = String, Path, description = "The environment's name."), ListQuery),
    responses(
        (status = 200, description = "One page of deployments, newest first.", body = ControlListPage<Deployment>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_deployments(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<Deployment>>> {
    let (app, env) = super::apps::resolve_app_environment(&state, &app_id, &env_name).await?;
    let collection = "environment-deployments";
    let binding = format!(
        "{};filter=all;sort=created-at-desc,canonical-id-desc-v1",
        environment_scope(app.canonical_id, env.canonical_id)
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .store
        .list_deployments_page(&env.id, limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |deployment| PagePosition {
            sort_value: deployment.created_at,
            identity: deployment.canonical_id.to_string(),
            secondary: None,
        },
    )
    .await?;
    Ok(Json(response))
}

async fn coordinate_strict_deployment(
    state: &CatalogState,
    env: &Environment,
    version: &Version,
    payload: &mut DeploymentPayload,
    note: Option<String>,
    reason: DeliveryReason,
) -> ApiResult<(Option<Deployment>, DeploymentPlan, DeliveryReport)> {
    coordinate_plan(state, env, version, payload, note, reason, None).await
}

pub(super) async fn coordinate_secret_change(
    state: &CatalogState,
    env: &Environment,
    version: &Version,
    payload: &mut DeploymentPayload,
    name: &str,
    replacement_revision: Option<&str>,
    expected_revision: Option<&str>,
) -> ApiResult<(Option<Deployment>, DeploymentPlan, DeliveryReport)> {
    coordinate_plan(
        state,
        env,
        version,
        payload,
        Some(format!("secret {name} revision changed")),
        DeliveryReason::SecretChange,
        Some(serde_json::json!({
            "name": name,
            "replacementRevision": replacement_revision,
            "expectedRevision": expected_revision,
        })),
    )
    .await
}

async fn coordinate_plan(
    state: &CatalogState,
    env: &Environment,
    version: &Version,
    payload: &mut DeploymentPayload,
    note: Option<String>,
    reason: DeliveryReason,
    secret_change: Option<serde_json::Value>,
) -> ApiResult<(Option<Deployment>, DeploymentPlan, DeliveryReport)> {
    let expected_secret_heads = payload.expected_secret_heads().unwrap_or_default();
    let target_ids = state.deliverer.serving_target_ids();
    let mut proxy_ids: Vec<String> = state
        .sessions
        .snapshot()
        .into_iter()
        .filter(|session| matches!(&session.node_role, NodeRole::Proxy(_)))
        .map(|session| session.node_id.clone())
        .collect();
    proxy_ids.sort();
    proxy_ids.dedup();
    let operation_kind = match reason {
        DeliveryReason::Deployment => "deployment",
        DeliveryReason::Rollback => "rollback",
        DeliveryReason::SecretChange => "secret_change",
    };
    let queue_fence = state.deliverer.requires_consumer_fence(version)?;
    let coordinator_id = CanonicalId::new_v7();

    let plan = state
        .store
        .create_deployment_plan(
            env,
            version,
            DeploymentPlanSpec {
                snapshot: payload.environment_snapshot().cloned(),
                expected_secret_heads,
                compare_secret_heads: reason != DeliveryReason::Rollback && secret_change.is_none(),
                target_ids,
                proxy_ids,
                queue_fence,
                operation_kind: operation_kind.to_string(),
                secret_change,
                note,
            },
            coordinator_id,
            DEPLOYMENT_COORDINATOR_LEASE_SECONDS,
        )
        .await?;
    let plan_id = plan.id.clone();
    let lease = DeploymentPlanLease::start(state.store.clone(), plan_id.clone(), coordinator_id);
    let result = async {
        payload.set_instruction(
            &plan.id,
            plan.routing_generation.to_string(),
            crate::runtime_client::proto::DeploymentAction::Prepare,
        );
        lease.ensure().await?;
        let prepared = state
            .deliverer
            .push_to(payload, reason, &plan.target_ids)
            .await;
        record_target_report(state, &plan, &prepared, "prepared", &lease).await?;
        if !prepared.fully_delivered() {
            let detail = delivery_failure_detail(&prepared);
            let aborting = state
                .store
                .transition_deployment_plan(
                    &plan.id,
                    "preparing",
                    "aborting",
                    Some(&detail),
                    lease.coordinator_id(),
                )
                .await?;
            let _ = abort_prepared_plan(state, env, &aborting, payload, reason, &lease).await?;
            return Err(ApiError::ServiceUnavailable(format!(
                "deployment plan {} failed during prepare; the production pointer was not changed: {}",
                plan.id, detail
            )));
        }

        let plan = state
            .store
            .transition_deployment_plan(
                &plan.id,
                "preparing",
                "prepared",
                Some("every frozen runtime target acknowledged the exact candidate generation"),
                lease.coordinator_id(),
            )
            .await?;
        if !prepare_queue_fence(state, env, &plan, &lease).await? {
            return Ok((None, state.store.deployment_plan(&plan.id).await?, prepared));
        }
        lease.ensure().await?;
        let deployment = match state
            .store
            .commit_deployment_plan(
                &plan,
                env,
                payload.environment_snapshot(),
                lease.coordinator_id(),
            )
            .await
        {
            Ok(deployment) => deployment,
            Err(error) => {
                if matches!(
                    &error,
                    CatalogError::Conflict(_)
                        | CatalogError::RevisionConflict(_)
                        | CatalogError::Invalid(_)
                ) {
                    let detail = error.to_string();
                    let aborting = state
                        .store
                        .transition_deployment_plan(
                            &plan.id,
                            "prepared",
                            "aborting",
                            Some(&detail),
                            lease.coordinator_id(),
                        )
                        .await?;
                    let _ =
                        abort_prepared_plan(state, env, &aborting, payload, reason, &lease).await?;
                }
                return Err(error.into());
            }
        };

        // A prepared generation already serves exact HTTP requests. Keep node
        // current selectors and asynchronous triggers on the baseline until every
        // frozen proxy proves the new routing generation is locally published.
        if !plan.proxy_ids.is_empty() {
            return Ok((
                Some(deployment),
                state.store.deployment_plan(&plan.id).await?,
                prepared,
            ));
        }

        let activating_plan = state.store.deployment_plan(&plan.id).await?;
        if !activate_queue_fence(state, env, &activating_plan, &lease).await? {
            return Ok((
                Some(deployment),
                state.store.deployment_plan(&plan.id).await?,
                prepared,
            ));
        }

        payload.set_instruction(
            &plan.id,
            plan.routing_generation.to_string(),
            crate::runtime_client::proto::DeploymentAction::Activate,
        );
        lease.ensure().await?;
        let activated = state
            .deliverer
            .push_to(payload, reason, &plan.target_ids)
            .await;
        record_target_report(state, &plan, &activated, "active", &lease).await?;
        if !activated.fully_delivered() {
            let detail = delivery_failure_detail(&activated);
            state
                .store
                .transition_deployment_plan(
                    &plan.id,
                    "activating",
                    "reconciling",
                    Some(&detail),
                    lease.coordinator_id(),
                )
                .await?;
        }
        let plan = finalize_if_observed(state, &plan.id, &lease).await?;
        Ok((Some(deployment), plan, activated))
    }
    .await;
    lease.finish().await;
    match result {
        Ok((deployment, _, delivery)) => Ok((
            deployment,
            state.store.deployment_plan(&plan_id).await?,
            delivery,
        )),
        Err(error) => Err(error),
    }
}

async fn record_target_report(
    state: &CatalogState,
    plan: &DeploymentPlan,
    report: &DeliveryReport,
    delivered_state: &str,
    lease: &DeploymentPlanLease,
) -> ApiResult<()> {
    for node_id in &report.delivered {
        state
            .store
            .record_deployment_target_observation(
                DeploymentTargetObservation {
                    plan_id: plan.id.clone(),
                    node_id: node_id.clone(),
                    observed_state: delivered_state.to_string(),
                    version_id: Some(plan.version_id.clone()),
                    routing_generation: Some(plan.routing_generation.to_string()),
                    message: None,
                    observed_at: now_secs(),
                },
                lease.coordinator_id(),
            )
            .await?;
    }
    for failure in &report.failed {
        state
            .store
            .record_deployment_target_observation(
                DeploymentTargetObservation {
                    plan_id: plan.id.clone(),
                    node_id: failure.node.clone(),
                    observed_state: "unconfirmed".to_string(),
                    version_id: Some(plan.version_id.clone()),
                    routing_generation: Some(plan.routing_generation.to_string()),
                    message: Some(failure.error.clone()),
                    observed_at: now_secs(),
                },
                lease.coordinator_id(),
            )
            .await?;
    }
    Ok(())
}

fn delivery_failure_detail(report: &DeliveryReport) -> String {
    if let Some(skipped) = &report.skipped {
        return skipped.clone();
    }
    report
        .failed
        .iter()
        .map(|failure| format!("{}: {}", failure.node, failure.error))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Advance `active` only from durable observations, never merely because a
/// pointer row exists. Called after runtime activation and after each proxy
/// reports a locally published routing table.
async fn finalize_if_observed(
    state: &CatalogState,
    plan_id: &str,
    lease: &DeploymentPlanLease,
) -> ApiResult<DeploymentPlan> {
    let plan = state.store.deployment_plan(plan_id).await?;
    if plan.state == "active" {
        return Ok(plan);
    }
    if !matches!(
        plan.state.as_str(),
        "activating" | "reconciling" | "degraded"
    ) {
        return Ok(plan);
    }
    let generation = plan.routing_generation.to_string();
    abandon_departed_participants(state, &plan, &generation, lease).await?;
    let targets = state.store.deployment_target_observations(plan_id).await?;
    let proxies = state.store.deployment_proxy_observations(plan_id).await?;
    // Counted rather than folded into the `all` below, because `all` stops at
    // the first target that is not settled: a count taken inside it would be
    // whatever the iteration happened to reach, which is right only in the
    // case where it is not needed.
    let abandoned = plan
        .target_ids
        .iter()
        .filter(|target| {
            targets
                .iter()
                .any(|observation| target_is(observation, target, ABANDONED, &plan, &generation))
        })
        .count();
    let all_targets_settled = plan.target_ids.iter().all(|target| {
        targets.iter().any(|observation| {
            target_is(observation, target, "active", &plan, &generation)
                || target_is(observation, target, ABANDONED, &plan, &generation)
        })
    });
    let all_proxies_applied = plan.proxy_ids.iter().all(|proxy| {
        proxies.iter().any(|observation| {
            observation.proxy_id == *proxy
                && observation.routing_generation == generation
                && (observation.observed_state == "routing_applied"
                    || observation.observed_state == ABANDONED)
        })
    });
    let queue_active = if plan_requires_queue_fence(&plan)? {
        state
            .store
            .deployment_queue_observation(plan_id)
            .await?
            .is_some_and(|observation| {
                observation.observed_state == "active"
                    && observation.active_generation == generation
                    && observation.active_deployment_id == plan.id
                    && observation.pending_generation.is_none()
                    && observation.pending_deployment_id.is_none()
            })
    } else {
        true
    };
    if all_targets_settled && all_proxies_applied && queue_active {
        let detail = if abandoned == 0 {
            "all frozen runtime and proxy targets report the exact production generation"
                .to_string()
        } else {
            // Said in the plan's own note, not only in the observation rows,
            // because this is the one place a reader looking at why the plan
            // ended would otherwise see nothing but "active".
            format!(
                "every frozen proxy target and every runtime target still on the fleet reports \
                 the exact production generation; {abandoned} runtime target(s) left the fleet \
                 before confirming and were recorded as abandoned rather than confirmed"
            )
        };
        return Ok(state
            .store
            .transition_deployment_plan(
                &plan.id,
                &plan.state,
                "active",
                Some(&detail),
                lease.coordinator_id(),
            )
            .await?);
    }
    Ok(plan)
}

/// Whether `observation` is this target's row for this plan, in `wanted`
/// state, about the exact version and generation this plan committed.
///
/// One predicate for both readers of that question - the finaliser deciding
/// whether a plan may end, and the abandoner deciding whether a target still
/// needs a verdict - because two spellings of it that drifted apart would
/// produce a plan that the finaliser says is unsettled and the abandoner says
/// needs nothing, which is the wedge this work exists to remove, reintroduced.
fn target_is(
    observation: &DeploymentTargetObservation,
    target: &str,
    wanted: &str,
    plan: &DeploymentPlan,
    generation: &str,
) -> bool {
    observation.node_id == target
        && observation.observed_state == wanted
        && observation.version_id.as_deref() == Some(plan.version_id.as_str())
        && observation.routing_generation.as_deref() == Some(generation)
}

/// What a target is recorded as when it left the fleet before confirming.
///
/// Deliberately not `active`: nothing observed this machine running this
/// version, and a plan that wrote `active` here would be answering a question
/// nobody asked it. It is also not `unconfirmed`, which is what a failed push
/// to a machine that is still on the fleet records - that one is expected to
/// be retried and to change. This one will not change, because there is
/// nothing left to retry against.
const ABANDONED: &str = "abandoned";

/// Whether the only targets that did not take the abort are machines that have
/// left the fleet, and have been gone long enough to give up on.
///
/// The compensation path has the same shape of hole the post-commit path had,
/// reached from the other side. A plan in `aborting` is pre-commit: nothing of
/// it is live, and finishing it costs nothing but the acknowledgement. But
/// `abort_prepared_plan` only transitions to `failed` when every frozen target
/// took the abort, and a machine that has gone takes nothing - so the plan
/// stays in `aborting`, which is in the set that refuses the next deployment
/// to that environment. The same environment held for the same reason, by a
/// plan that never even changed anything.
///
/// A target that is still on the fleet and refusing is not covered, exactly as
/// on the other path: that is a machine with a problem, and it must keep being
/// retried and keep being visible.
///
/// The row written here says `abandoned` rather than `aborted`, because
/// nothing observed this machine dropping the prepared version. What it
/// actually has is a staged version that was never activated, which its own
/// next reconcile discards - so the compensation does happen, just not where
/// this plan could watch it.
async fn only_departed_targets_are_left(
    state: &CatalogState,
    plan: &DeploymentPlan,
    report: &DeliveryReport,
    lease: &DeploymentPlanLease,
) -> ApiResult<bool> {
    if report.failed.is_empty() {
        return Ok(false);
    }
    let deadline = state.abandon_departed_target_after.as_secs() as i64;
    let now = now_secs();
    let age = now.saturating_sub(plan.created_at);
    // The same two clocks the post-commit path uses, and the same reason: the
    // fleet's own record of when it dropped a machine answers "how long has it
    // been gone", and the plan's age is the fallback for a name there is no
    // such record for.
    let gone_for = |node: &str| match state.nodes.departed_at(node) {
        Some(at) => now.saturating_sub(at.timestamp()),
        None => age,
    };
    if report
        .failed
        .iter()
        .any(|failure| state.nodes.node(&failure.node).is_some() || gone_for(&failure.node) < deadline)
    {
        return Ok(false);
    }

    for failure in &report.failed {
        error!(
            plan = %plan.id,
            node = %failure.node,
            app_id = %plan.app_id,
            env_id = %plan.env_id,
            gone_for_secs = gone_for(&failure.node),
            error = %failure.error,
            "A target this deployment had already prepared left the fleet before acknowledging \
             the abort, and has not come back within the deadline, so it is recorded as abandoned \
             and this plan is allowed to fail rather than stay in aborting for ever. Nothing was \
             activated on that machine - the plan never committed - and a node discards a staged \
             version it is not assigned the next time it reconciles, so the compensation still \
             happens; what is given up here is watching it happen"
        );
        state
            .store
            .record_deployment_target_observation(
                DeploymentTargetObservation {
                    plan_id: plan.id.clone(),
                    node_id: failure.node.clone(),
                    observed_state: ABANDONED.to_string(),
                    version_id: Some(plan.version_id.clone()),
                    routing_generation: Some(plan.routing_generation.to_string()),
                    message: Some(format!(
                        "off the fleet for {}s, past the {deadline}s deadline; never acknowledged \
                         the abort",
                        gone_for(&failure.node)
                    )),
                    observed_at: now_secs(),
                },
                lease.coordinator_id(),
            )
            .await?;
    }
    Ok(true)
}

/// The same rule for the other half of a plan's frozen participants: a proxy
/// that is no longer a live session.
///
/// It is a separate function only because a proxy's presence is a different
/// question - a live gRPC session rather than a row on the fleet list - and
/// the answer is read from a different place. The hole it closes is the same
/// one, and closing only the runtime half would have left a plan that can be
/// held for ever by a proxy instead of by a node, which is not a fix.
///
/// A proxy's frozen list is taken from the sessions that were live when the
/// plan was made, so "not a live session now" is exactly the negation of what
/// put the name there. A restarting proxy reconnects and reports, which is why
/// the same deadline applies here: it is the interval that separates "gone for
/// a moment" from "gone".
async fn abandon_departed_proxies(
    state: &CatalogState,
    plan: &DeploymentPlan,
    generation: &str,
    age: i64,
) -> ApiResult<()> {
    if plan.proxy_ids.is_empty() {
        return Ok(());
    }
    let live: std::collections::HashSet<String> = state
        .sessions
        .snapshot()
        .into_iter()
        .filter(|session| matches!(&session.node_role, NodeRole::Proxy(_)))
        .map(|session| session.node_id.clone())
        .collect();
    let observations = state.store.deployment_proxy_observations(&plan.id).await?;

    for proxy in &plan.proxy_ids {
        let settled = observations.iter().any(|observation| {
            observation.proxy_id == *proxy
                && observation.routing_generation == generation
                && (observation.observed_state == "routing_applied"
                    || observation.observed_state == ABANDONED)
        });
        if settled || live.contains(proxy) {
            continue;
        }

        error!(
            plan = %plan.id,
            proxy = %proxy,
            app_id = %plan.app_id,
            env_id = %plan.env_id,
            generation = %generation,
            waited_secs = age,
            "A proxy this deployment froze as a participant is no longer connected and has not \
             reported this routing generation within the deadline, so it is recorded as abandoned \
             and this plan is allowed to finish. Nothing observed that proxy publishing this \
             table. A proxy that reconnects fetches the whole routing table on its next poll, so \
             what was lost here is the confirmation and not the table itself; a proxy that never \
             reconnects was not going to serve this generation whatever this plan did"
        );

        state
            .store
            .record_deployment_proxy_observation(
                DeploymentProxyObservation {
                    plan_id: plan.id.clone(),
                    proxy_id: proxy.clone(),
                    observed_state: ABANDONED.to_string(),
                    routing_table_version: String::new(),
                    routing_generation: generation.to_string(),
                    observed_at: now_secs(),
                })
            .await?;
    }
    Ok(())
}

/// Record every frozen target that has left the fleet and run out of time, so
/// a committed plan can reach a terminal state instead of holding its
/// environment for ever.
///
/// # Why a plan could not finish at all
///
/// `finalize_if_observed` advances to `active` only when *every* frozen target
/// has reported the exact generation. The target list is frozen when the plan
/// is created and keeps names that have since gone quiet, which is right: a
/// node that is rebooting must still be pushed to when it answers. But a
/// machine that never answers again can never report anything, so that
/// universal quantifier is never satisfied, and the plan stays in
/// `activating`, `reconciling` or `degraded` - all three of which are in the
/// set that refuses the next deployment to that environment with "a deployment
/// plan is already in progress". No operator action clears it: the coordinator
/// does not die, it succeeds at an impossible action every two seconds, and
/// deregistering the node turns the push failure from a transport error into
/// an unknown-node error without making it any less of a failure.
///
/// # What this does and does not decide
///
/// Only a target that is **not on the fleet at this moment** is eligible. A
/// target that is listed and failing is waited for indefinitely, unchanged: it
/// is a machine with a problem, and giving up on it would be this function
/// papering over exactly what an operator needs to see.
///
/// The deadline is measured against how long the machine has been gone, taken
/// from the fleet's own record of when it dropped it, and against the plan's
/// age only when there is no such record. See the comment at the measurement
/// for why the plan's clock alone would make every deployment after a machine
/// was switched off take the whole deadline.
async fn abandon_departed_participants(
    state: &CatalogState,
    plan: &DeploymentPlan,
    generation: &str,
    lease: &DeploymentPlanLease,
) -> ApiResult<()> {
    let deadline = state.abandon_departed_target_after.as_secs() as i64;
    let now = now_secs();
    let age = now.saturating_sub(plan.created_at);

    // The proxy half has only the plan's own clock to measure with, so it is
    // still gated on the plan being old enough.
    if age >= deadline {
        abandon_departed_proxies(state, plan, generation, age).await?;
    }

    let observations = state.store.deployment_target_observations(&plan.id).await?;
    for target in &plan.target_ids {
        let settled = observations.iter().any(|observation| {
            target_is(observation, target, "active", plan, generation)
                || target_is(observation, target, ABANDONED, plan, generation)
        });
        if settled || state.nodes.node(target).is_some() {
            continue;
        }

        // How long this machine has been gone, from the better of two clocks.
        //
        // The fleet remembers when it dropped a node, and that is the clock
        // that answers the question actually being asked. Measuring from the
        // plan instead would restart the wait at every deployment: a machine
        // that has been gone since Tuesday is frozen into every plan made
        // since - `serving_target_ids` keeps naming it on purpose, so that a
        // deploy says which machine it could not reach - and each of those
        // plans would sit out the whole deadline again before it could end.
        // One machine switched off would make every deployment take ten
        // minutes, for ever.
        //
        // The plan's own age is the fallback, for a name the fleet has no
        // departure record for: one lost before this process started, or
        // cleared by `DELETE .../registration` while a plan still named it.
        let gone_for = match state.nodes.departed_at(target) {
            Some(at) => now.saturating_sub(at.timestamp()),
            None => age,
        };
        if gone_for < deadline {
            continue;
        }

        error!(
            plan = %plan.id,
            node = %target,
            app_id = %plan.app_id,
            env_id = %plan.env_id,
            generation = %generation,
            gone_for_secs = gone_for,
            "A deployment target left the fleet before confirming this generation and has not \
             come back within the deadline, so it is recorded as abandoned and this plan is \
             allowed to finish. Nothing observed this machine running this version, and nothing \
             will: the plan's target list keeps the name, and the observation row says abandoned \
             rather than active. If that machine returns it asks the control plane what it \
             should be running and loads the current version from the answer, so what was lost \
             here is the confirmation and not the delivery. Raise              deployment_target_abandon_after_secs if machines on this installation take longer \
             than this to come back"
        );

        state
            .store
            .record_deployment_target_observation(
                DeploymentTargetObservation {
                    plan_id: plan.id.clone(),
                    node_id: target.clone(),
                    observed_state: ABANDONED.to_string(),
                    version_id: Some(plan.version_id.clone()),
                    routing_generation: Some(generation.to_string()),
                    message: Some(format!(
                        "off the fleet for {gone_for}s, past the {deadline}s deadline; never \
                         confirmed this generation"
                    )),
                    observed_at: now_secs(),
                },
                lease.coordinator_id(),
            )
            .await?;
    }
    Ok(())
}

/// Resume every plan whose outcome was not durably terminal. All wire actions
/// are idempotent and carry the exact plan/generation, so replay is safer than
/// guessing which side of a lost response completed.
pub(crate) fn resume_on_start(state: CatalogState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut next_secret_revision_gc = 0;
        let mut next_provider_credential_lease_reap = 0;
        loop {
            interval.tick().await;
            let plans = match state.store.nonterminal_deployment_plans().await {
                Ok(plans) => plans,
                Err(error) => {
                    warn!(error = %error, "Could not list deployment plans for reconciliation");
                    continue;
                }
            };
            for plan in plans {
                if let Err(error) = reconcile_plan(&state, plan.clone()).await {
                    warn!(
                        deployment_plan = %plan.id,
                        state = %plan.state,
                        error = ?error,
                        "Deployment reconciliation pass did not converge; the durable plan remains visible and will be retried"
                    );
                }
            }
            let cycle_now = now_secs();
            if cycle_now >= next_secret_revision_gc {
                match state
                    .store
                    .reap_unreferenced_secret_revisions(cycle_now)
                    .await
                {
                    Ok(removed) => {
                        if !removed.is_empty() {
                            info!(
                                revisions = removed.len(),
                                "Destroyed expired, unreferenced staged secret ciphertext and recorded immutable evidence"
                            );
                        }
                        next_secret_revision_gc =
                            cycle_now.saturating_add(SECRET_REVISION_GC_INTERVAL_SECONDS);
                    }
                    Err(error) => warn!(
                        error = %error,
                        "Could not reconcile staged-secret retention; ciphertext was retained and the next pass will retry"
                    ),
                }
            }
            if cycle_now >= next_provider_credential_lease_reap {
                match state.provider_credentials.expire_leases().await {
                    Ok(expired) => {
                        if expired > 0 {
                            info!(
                                expired,
                                "Expired provider credential leases and recorded immutable audit events"
                            );
                        }
                        next_provider_credential_lease_reap = cycle_now.saturating_add(30);
                    }
                    Err(error) => warn!(
                        error = %error,
                        "Could not expire provider credential leases; no credential was exposed and the next pass will retry"
                    ),
                }
            }
            let drains = match state
                .store
                .due_deployment_generation_drains(now_secs())
                .await
            {
                Ok(drains) => drains,
                Err(error) => {
                    warn!(error = %error, "Could not list retained deployment generations due for drain");
                    continue;
                }
            };
            for drain in drains {
                if let Err(error) = reconcile_generation_drain(&state, drain.clone()).await {
                    warn!(
                        deployment_plan = %drain.plan_id,
                        routing_generation = %drain.generation,
                        error = ?error,
                        "Retained deployment generation did not drain; it remains durably pending"
                    );
                }
            }
        }
    });
}

async fn reconcile_generation_drain(
    state: &CatalogState,
    drain: DeploymentGenerationDrain,
) -> ApiResult<()> {
    let coordinator_id = CanonicalId::new_v7();
    let plan = match state
        .store
        .claim_deployment_generation_drain(
            &drain.plan_id,
            coordinator_id,
            DEPLOYMENT_COORDINATOR_LEASE_SECONDS,
        )
        .await
    {
        Ok(plan) => plan,
        Err(CatalogError::Conflict(_)) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let lease = DeploymentPlanLease::start(state.store.clone(), plan.id.clone(), coordinator_id);
    let result = reconcile_claimed_generation_drain(state, drain, plan, &lease).await;
    lease.finish().await;
    result
}

async fn reconcile_claimed_generation_drain(
    state: &CatalogState,
    drain: DeploymentGenerationDrain,
    plan: DeploymentPlan,
    lease: &DeploymentPlanLease,
) -> ApiResult<()> {
    let env = state.store.get_environment_by_id(&plan.env_id).await?;
    let version = state
        .store
        .get_version(&plan.app_id, &plan.version_id)
        .await?;
    let base_url = state.deliverer.machine_reconciliation_base_url();
    let mut payload = if let Some(snapshot_id) = &plan.snapshot_id {
        let snapshot = state.store.get_environment_snapshot(snapshot_id).await?;
        state
            .deliverer
            .payload_for_snapshot(&env, &version, &base_url, snapshot)
            .await?
    } else {
        state.deliverer.payload(&env, &version, &base_url).await?
    };
    payload.set_instruction(
        &plan.id,
        &drain.generation,
        crate::runtime_client::proto::DeploymentAction::Drain,
    );
    state
        .store
        .set_deployment_generation_drain_state(&plan.id, "draining", None, lease.coordinator_id())
        .await?;
    lease.ensure().await?;
    let report = state
        .deliverer
        .push_to(&payload, plan_delivery_reason(&plan)?, &drain.target_ids)
        .await;
    for node_id in &report.delivered {
        state
            .store
            .record_deployment_drain_observation(
                DeploymentDrainObservation {
                    plan_id: plan.id.clone(),
                    node_id: node_id.clone(),
                    generation: drain.generation.clone(),
                    observed_state: "drained".to_string(),
                    message: None,
                    observed_at: now_secs(),
                },
                lease.coordinator_id(),
            )
            .await?;
    }
    for failure in &report.failed {
        state
            .store
            .record_deployment_drain_observation(
                DeploymentDrainObservation {
                    plan_id: plan.id.clone(),
                    node_id: failure.node.clone(),
                    generation: drain.generation.clone(),
                    observed_state: "draining".to_string(),
                    message: Some(failure.error.clone()),
                    observed_at: now_secs(),
                },
                lease.coordinator_id(),
            )
            .await?;
    }
    if report.fully_delivered() {
        state
            .store
            .set_deployment_generation_drain_state(
                &plan.id,
                "drained",
                None,
                lease.coordinator_id(),
            )
            .await?;
    } else {
        state
            .store
            .set_deployment_generation_drain_state(
                &plan.id,
                "draining",
                Some(&delivery_failure_detail(&report)),
                lease.coordinator_id(),
            )
            .await?;
    }
    Ok(())
}

pub(super) fn reconcile_soon(state: CatalogState, plan: DeploymentPlan) {
    tokio::spawn(async move {
        if let Err(error) = reconcile_plan(&state, plan.clone()).await {
            warn!(
                deployment_plan = %plan.id,
                state = %plan.state,
                error = ?error,
                "Immediate deployment reconciliation did not converge; the periodic reconciler will retry"
            );
        }
    });
}

async fn reconcile_plan(state: &CatalogState, plan: DeploymentPlan) -> ApiResult<()> {
    let coordinator_id = CanonicalId::new_v7();
    let plan = match state
        .store
        .claim_deployment_plan(
            &plan.id,
            coordinator_id,
            DEPLOYMENT_COORDINATOR_LEASE_SECONDS,
        )
        .await
    {
        Ok(plan) => plan,
        // Another request or reconciliation pass is making progress. This is
        // expected under both HA and the local immediate/periodic race.
        Err(CatalogError::Conflict(_)) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let lease = DeploymentPlanLease::start(state.store.clone(), plan.id.clone(), coordinator_id);
    let result = reconcile_claimed_plan(state, plan, &lease).await;
    lease.finish().await;
    result
}

async fn reconcile_claimed_plan(
    state: &CatalogState,
    plan: DeploymentPlan,
    lease: &DeploymentPlanLease,
) -> ApiResult<()> {
    if plan.target_ids.is_empty() {
        let aborting = match plan.state.as_str() {
            "preparing" | "prepared" => {
                state
                    .store
                    .transition_deployment_plan(
                        &plan.id,
                        &plan.state,
                        "aborting",
                        Some("strict deployment plan has no runtime serving target"),
                        lease.coordinator_id(),
                    )
                    .await?
            }
            "aborting" => plan.clone(),
            state_name => {
                return Err(ApiError::Conflict(format!(
                    "deployment plan {} has no runtime serving target after entering post-commit state {state_name}",
                    plan.id
                )));
            }
        };
        state
            .store
            .transition_deployment_plan(
                &aborting.id,
                "aborting",
                "failed",
                Some("strict deployment plan has no runtime serving target"),
                lease.coordinator_id(),
            )
            .await?;
        return Ok(());
    }
    let env = state.store.get_environment_by_id(&plan.env_id).await?;
    let version = state
        .store
        .get_version(&plan.app_id, &plan.version_id)
        .await?;
    let base_url = state.deliverer.machine_reconciliation_base_url();
    let mut payload = if let Some(snapshot_id) = &plan.snapshot_id {
        let snapshot = state.store.get_environment_snapshot(snapshot_id).await?;
        state
            .deliverer
            .payload_for_snapshot(&env, &version, &base_url, snapshot)
            .await?
    } else {
        state.deliverer.payload(&env, &version, &base_url).await?
    };
    let reason = plan_delivery_reason(&plan)?;

    match plan.state.as_str() {
        "preparing" => {
            payload.set_instruction(
                &plan.id,
                plan.routing_generation.to_string(),
                crate::runtime_client::proto::DeploymentAction::Prepare,
            );
            lease.ensure().await?;
            let report = state
                .deliverer
                .push_to(&payload, reason, &plan.target_ids)
                .await;
            record_target_report(state, &plan, &report, "prepared", lease).await?;
            if report.fully_delivered() {
                state
                    .store
                    .transition_deployment_plan(
                        &plan.id,
                        "preparing",
                        "prepared",
                        Some("restart reconciliation confirmed every prepared target"),
                        lease.coordinator_id(),
                    )
                    .await?;
            }
        }
        "prepared" => {
            if !prepare_queue_fence(state, &env, &plan, lease).await? {
                return Ok(());
            }
            lease.ensure().await?;
            match state
                .store
                .commit_deployment_plan(
                    &plan,
                    &env,
                    payload.environment_snapshot(),
                    lease.coordinator_id(),
                )
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    if matches!(
                        &error,
                        CatalogError::Conflict(_)
                            | CatalogError::RevisionConflict(_)
                            | CatalogError::Invalid(_)
                    ) {
                        let detail = error.to_string();
                        let aborting = state
                            .store
                            .transition_deployment_plan(
                                &plan.id,
                                "prepared",
                                "aborting",
                                Some(&detail),
                                lease.coordinator_id(),
                            )
                            .await?;
                        let _ = abort_prepared_plan(
                            state,
                            &env,
                            &aborting,
                            &mut payload,
                            reason,
                            lease,
                        )
                        .await?;
                    }
                }
            }
        }
        "aborting" => {
            let _ = abort_prepared_plan(state, &env, &plan, &mut payload, reason, lease).await?;
        }
        "activating" | "reconciling" | "degraded" => {
            // Before the proxy gate, not after it. A plan waiting on a proxy
            // that has gone returns from that gate and never reaches
            // `finalize_if_observed`, so an abandonment that only happened
            // there would close the runtime half of the hole and leave the
            // proxy half exactly as it was.
            abandon_departed_participants(
                state,
                &plan,
                &plan.routing_generation.to_string(),
                lease,
            )
            .await?;
            if !all_proxies_observed(state, &plan).await? {
                return Ok(());
            }
            if !activate_queue_fence(state, &env, &plan, lease).await? {
                return Ok(());
            }
            payload.set_instruction(
                &plan.id,
                plan.routing_generation.to_string(),
                crate::runtime_client::proto::DeploymentAction::Activate,
            );
            lease.ensure().await?;
            let report = state
                .deliverer
                .push_to(&payload, reason, &plan.target_ids)
                .await;
            record_target_report(state, &plan, &report, "active", lease).await?;
            if !report.fully_delivered() && plan.state == "activating" {
                state
                    .store
                    .transition_deployment_plan(
                        &plan.id,
                        "activating",
                        "reconciling",
                        Some(&delivery_failure_detail(&report)),
                        lease.coordinator_id(),
                    )
                    .await?;
            }
            finalize_if_observed(state, &plan.id, lease).await?;
        }
        _ => {}
    }
    Ok(())
}

fn plan_delivery_reason(plan: &DeploymentPlan) -> ApiResult<DeliveryReason> {
    match plan
        .policy
        .get("operation")
        .and_then(serde_json::Value::as_str)
    {
        Some("deployment") => Ok(DeliveryReason::Deployment),
        Some("rollback") => Ok(DeliveryReason::Rollback),
        Some("secret_change") => Ok(DeliveryReason::SecretChange),
        _ => Err(ApiError::Conflict(format!(
            "deployment plan {} has no valid operation policy",
            plan.id
        ))),
    }
}

fn plan_requires_queue_fence(plan: &DeploymentPlan) -> ApiResult<bool> {
    plan.policy
        .get("queueFence")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| {
            ApiError::Conflict(format!(
                "deployment plan {} has no valid queueFence policy",
                plan.id
            ))
        })
}

async fn record_queue_observation(
    state: &CatalogState,
    lease: &DeploymentPlanLease,
    observation: DeploymentQueueObservation,
) -> ApiResult<()> {
    state
        .store
        .record_deployment_queue_observation(observation, lease.coordinator_id())
        .await?;
    Ok(())
}

/// Pre-commit half of the durable queue barrier. It closes all Pull admission
/// and waits for valid old leases, but cannot make the candidate active.
async fn prepare_queue_fence(
    state: &CatalogState,
    env: &Environment,
    plan: &DeploymentPlan,
    lease: &DeploymentPlanLease,
) -> ApiResult<bool> {
    if !plan_requires_queue_fence(plan)? {
        return Ok(true);
    }
    lease.ensure().await?;
    match state.deliverer.prepare_consumer_generation(env, plan).await {
        Ok(progress) => {
            let observed_state = if progress.prepared {
                "prepared"
            } else {
                "draining"
            };
            record_queue_observation(
                state,
                lease,
                DeploymentQueueObservation {
                    plan_id: plan.id.clone(),
                    observed_state: observed_state.to_string(),
                    active_generation: progress.active_generation.clone(),
                    active_deployment_id: progress.active_deployment_id.clone(),
                    pending_generation: (!progress.pending_generation.is_empty())
                        .then(|| progress.pending_generation.clone()),
                    pending_deployment_id: (!progress.pending_deployment_id.is_empty())
                        .then(|| progress.pending_deployment_id.clone()),
                    outstanding_leases: progress.outstanding_leases,
                    message: None,
                    observed_at: now_secs(),
                },
            )
            .await?;
            info!(
                deployment_plan = %plan.id,
                routing_generation = %plan.routing_generation,
                outstanding_leases = progress.outstanding_leases,
                prepared = progress.prepared,
                "Queue admission is closed at the pre-commit generation barrier"
            );
            Ok(progress.prepared)
        }
        Err(error) => {
            record_queue_observation(
                state,
                lease,
                DeploymentQueueObservation {
                    plan_id: plan.id.clone(),
                    observed_state: "error".to_string(),
                    active_generation: String::new(),
                    active_deployment_id: String::new(),
                    pending_generation: None,
                    pending_deployment_id: None,
                    outstanding_leases: 0,
                    message: Some(error.to_string()),
                    observed_at: now_secs(),
                },
            )
            .await?;
            warn!(
                deployment_plan = %plan.id,
                routing_generation = %plan.routing_generation,
                error = %error,
                "Queue generation prepare did not converge; production pointer remains unchanged and reconciliation will retry"
            );
            Ok(false)
        }
    }
}

/// Post-routing half of the queue barrier. At this point the pointer is
/// durable and every frozen proxy has published it, so admitting the new
/// generation cannot expose work ahead of HTTP routing.
async fn activate_queue_fence(
    state: &CatalogState,
    env: &Environment,
    plan: &DeploymentPlan,
    lease: &DeploymentPlanLease,
) -> ApiResult<bool> {
    if !plan_requires_queue_fence(plan)? {
        return Ok(true);
    }
    lease.ensure().await?;
    match state
        .deliverer
        .activate_consumer_generation(env, plan)
        .await
    {
        Ok(progress) => {
            record_queue_observation(
                state,
                lease,
                DeploymentQueueObservation {
                    plan_id: plan.id.clone(),
                    observed_state: if progress.activated {
                        "active".to_string()
                    } else {
                        "prepared".to_string()
                    },
                    active_generation: progress.active_generation.clone(),
                    active_deployment_id: progress.active_deployment_id.clone(),
                    pending_generation: (!progress.pending_generation.is_empty())
                        .then(|| progress.pending_generation.clone()),
                    pending_deployment_id: (!progress.pending_deployment_id.is_empty())
                        .then(|| progress.pending_deployment_id.clone()),
                    outstanding_leases: progress.outstanding_leases,
                    message: None,
                    observed_at: now_secs(),
                },
            )
            .await?;
            Ok(progress.activated)
        }
        Err(error) => {
            record_queue_observation(
                state,
                lease,
                DeploymentQueueObservation {
                    plan_id: plan.id.clone(),
                    observed_state: "error".to_string(),
                    active_generation: String::new(),
                    active_deployment_id: String::new(),
                    pending_generation: None,
                    pending_deployment_id: None,
                    outstanding_leases: 0,
                    message: Some(error.to_string()),
                    observed_at: now_secs(),
                },
            )
            .await?;
            warn!(
                deployment_plan = %plan.id,
                routing_generation = %plan.routing_generation,
                error = %error,
                "Queue generation activation is uncertain; runtime activation remains blocked and reconciliation will retry"
            );
            if plan.state == "activating" {
                state
                    .store
                    .transition_deployment_plan(
                        &plan.id,
                        "activating",
                        "reconciling",
                        Some(&format!("queue generation activation: {error}")),
                        lease.coordinator_id(),
                    )
                    .await?;
            }
            Ok(false)
        }
    }
}

/// Compensate only a plan that durably owns pre-commit abort state. Queue
/// admission is reopened before the candidate runtime generation is removed.
async fn abort_prepared_plan(
    state: &CatalogState,
    env: &Environment,
    plan: &DeploymentPlan,
    payload: &mut DeploymentPayload,
    reason: DeliveryReason,
    lease: &DeploymentPlanLease,
) -> ApiResult<bool> {
    if plan.state != "aborting" {
        return Err(ApiError::Conflict(format!(
            "deployment plan {} is {}, not aborting",
            plan.id, plan.state
        )));
    }
    if plan_requires_queue_fence(plan)? {
        lease.ensure().await?;
        match state.deliverer.abort_consumer_generation(env, plan).await {
            Ok(progress) => {
                record_queue_observation(
                    state,
                    lease,
                    DeploymentQueueObservation {
                        plan_id: plan.id.clone(),
                        observed_state: "aborted".to_string(),
                        active_generation: progress.active_generation,
                        active_deployment_id: progress.active_deployment_id,
                        pending_generation: None,
                        pending_deployment_id: None,
                        outstanding_leases: 0,
                        message: None,
                        observed_at: now_secs(),
                    },
                )
                .await?;
            }
            Err(error) => {
                record_queue_observation(
                    state,
                    lease,
                    DeploymentQueueObservation {
                        plan_id: plan.id.clone(),
                        observed_state: "error".to_string(),
                        active_generation: String::new(),
                        active_deployment_id: String::new(),
                        pending_generation: Some(plan.routing_generation.to_string()),
                        pending_deployment_id: Some(plan.id.clone()),
                        outstanding_leases: 0,
                        message: Some(error.to_string()),
                        observed_at: now_secs(),
                    },
                )
                .await?;
                return Ok(false);
            }
        }
    }
    payload.set_instruction(
        &plan.id,
        plan.routing_generation.to_string(),
        crate::runtime_client::proto::DeploymentAction::Abort,
    );
    lease.ensure().await?;
    let report = state
        .deliverer
        .push_to(payload, reason, &plan.target_ids)
        .await;
    record_target_report(state, plan, &report, "aborted", lease).await?;
    if !report.fully_delivered() && !only_departed_targets_are_left(state, plan, &report, lease).await? {
        return Ok(false);
    }
    let terminal = state
        .store
        .transition_deployment_plan(
            &plan.id,
            "aborting",
            "failed",
            plan.last_error
                .as_deref()
                .or(Some("pre-commit deployment aborted")),
            lease.coordinator_id(),
        )
        .await?;
    cleanup_aborted_secret_candidate(state, env, &terminal).await;
    Ok(true)
}

async fn cleanup_aborted_secret_candidate(
    state: &CatalogState,
    env: &Environment,
    plan: &DeploymentPlan,
) {
    let Some(change) = plan.policy.get("secretChange") else {
        return;
    };
    let (Some(name), Some(revision)) = (
        change.get("name").and_then(serde_json::Value::as_str),
        change
            .get("replacementRevision")
            .and_then(serde_json::Value::as_str),
    ) else {
        return;
    };
    match state
        .store
        .discard_secret_revision_if_unreferenced(&env.id, name, revision)
        .await
    {
        Ok(true) => info!(
            deployment_plan = %plan.id,
            secret = %name,
            revision = %revision,
            "Removed staged secret ciphertext after every pre-commit compensation was confirmed"
        ),
        Ok(false) => {}
        Err(error) => warn!(
            deployment_plan = %plan.id,
            secret = %name,
            revision = %revision,
            error = %error,
            "Could not remove terminal staged secret ciphertext; retention reconciliation must retry"
        ),
    }
}

async fn all_proxies_observed(state: &CatalogState, plan: &DeploymentPlan) -> ApiResult<bool> {
    let observations = state.store.deployment_proxy_observations(&plan.id).await?;
    let generation = plan.routing_generation.to_string();
    Ok(plan.proxy_ids.iter().all(|proxy| {
        observations.iter().any(|observation| {
            observation.proxy_id == *proxy
                && observation.routing_generation == generation
                // Abandoned counts as settled here for the same reason it does
                // for a runtime target: this proxy is not going to report, and
                // a gate that waits for it is a gate that never opens.
                && (observation.observed_state == "routing_applied"
                    || observation.observed_state == ABANDONED)
        })
    }))
}

/// Read the optional rollback body.
///
/// Taken as raw bytes on purpose. `Option<Json<T>>` turns *every* rejection
/// into `None`, so a typo in the payload would read as "no body given" and
/// silently roll production back to the previous version. Here an empty
/// body means exactly that and nothing else; anything that does not parse
/// is the caller's mistake and is reported as one.
///
/// Returns the reason it could not be read, so the caller can both answer
/// the client and say in the log why a production-changing request was
/// turned away.
fn parse_rollback_body(body: &[u8]) -> Result<RollbackRequest, String> {
    if body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(RollbackRequest::default());
    }
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    if !value.is_object() {
        // serde would happily read a JSON array into this struct and fill
        // every field with its default, which is indistinguishable from an
        // empty body and would roll the environment back regardless.
        return Err(format!("expected a JSON object, got {}", json_kind(&value)));
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

/// Name the JSON shape a caller sent, for an error message that says what
/// was wrong rather than only that something was.
fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

fn response(
    env: &Environment,
    deployment: Option<Deployment>,
    version: &Version,
    delivery: Option<DeliveryReport>,
    plan: Option<DeploymentPlan>,
) -> DeploymentResponse {
    DeploymentResponse {
        deployment,
        environment: env.name.clone(),
        version: VersionView::from(version),
        delivery,
        plan,
    }
}

/// Accept either form of "which version", and say so plainly when neither
/// was given.
async fn resolve_version(
    state: &CatalogState,
    app_id: &str,
    version_id: Option<&str>,
    version_number: Option<i64>,
) -> Result<Version, ApiError> {
    if let Some(version_id) = version_id {
        return Ok(state.store.get_version(app_id, version_id).await?);
    }
    if let Some(number) = version_number {
        return Ok(state.store.get_version_by_number(app_id, number).await?);
    }
    Err(ApiError::BadRequest(
        "name the version to deploy with either \"version_id\" or \"version_number\"".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        Harness, call, harness, harness_with_failed_runtime, harness_with_queue, json,
    };
    use super::{deployment_plan_location, reconcile_plan};
    use crate::api::catalog::MAX_BUNDLE_BYTES;
    use crate::api::catalog::routing_table;
    use crate::api::paths::ApiPaths;
    use crate::api::versioning::ApiSurface;
    use crate::catalog::DeploymentPlanSpec;
    use crate::catalog::bundle::tests::{bundle_with_manifest, sample_bundle};

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header::LOCATION};
    use axum::middleware::from_fn_with_state;
    use deis_wire::CanonicalId;
    use serde_json::Value;
    use tower::ServiceExt;

    #[test]
    fn mutation_location_identifies_the_durable_plan_status_resource() {
        let plan = "018f2518-7e38-7f80-8000-000000000001";
        let headers =
            deployment_plan_location(ApiPaths::on(ApiSurface::V1), "demo", "production", plan);
        assert_eq!(
            headers.get(LOCATION).unwrap(),
            "/api/v1/apps/demo/environments/production/deployment-plans/018f2518-7e38-7f80-8000-000000000001"
        );
    }

    /// The same deployment route, reached under the prefix the request
    /// context names, points the caller at the plan under that prefix: a
    /// v1 client is never handed a path this router does not serve. The
    /// harness carries no surface, so its answer is spelled for v1.
    #[tokio::test]
    async fn a_plan_location_is_spelled_for_the_surface_the_caller_used() {
        let h = harness(true).await;
        json(
            &h.router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let (_, uploaded) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("demo")),
            "application/zip",
        )
        .await;
        let version_id = uploaded["version"]["id"].as_str().unwrap().to_string();
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;

        // The versioned mount as the router builds it: the product routes
        // nested under the prefix, with the request context that names the
        // surface layered over the mount.
        let v1 = Router::new()
            .nest("/api/v1", super::super::router("", h.state.clone()))
            .layer(from_fn_with_state(
                ApiSurface::V1,
                crate::api::versioning::attach_request_context,
            ));

        for (router, uri, expected_prefix) in [
            (
                &h.router,
                "/api/apps/demo/environments/production/deployments",
                "/api/v1/apps/demo/environments/production/deployment-plans/",
            ),
            (
                &v1,
                "/api/v1/apps/demo/environments/production/deployments",
                "/api/v1/apps/demo/environments/production/deployment-plans/",
            ),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({"version_id": version_id}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                response.status().is_success(),
                "{uri}: {}",
                response.status()
            );
            let location = response.headers()[LOCATION].to_str().unwrap().to_string();
            let body: Value = serde_json::from_slice(
                &to_bytes(response.into_body(), MAX_BUNDLE_BYTES)
                    .await
                    .unwrap(),
            )
            .unwrap();
            let plan_id = body["plan"]["id"].as_str().unwrap();
            assert_eq!(location, format!("{expected_prefix}{plan_id}"), "{uri}");
        }
    }

    #[tokio::test]
    async fn request_owner_excludes_the_local_background_reconciler_until_release() {
        let h = harness(true).await;
        h.state.store.create_app("lease-app").await.unwrap();
        let env = h
            .state
            .store
            .create_environment("lease-app", "production")
            .await
            .unwrap();
        let (version, _) = h
            .state
            .store
            .create_version(
                "lease-app",
                "sha-lease",
                "/unused/lease.zip",
                r#"{"schema":1,"app":"lease-app","entry":"worker/index.js"}"#,
            )
            .await
            .unwrap();
        let request_owner = CanonicalId::new_v7();
        let plan = h
            .state
            .store
            .create_deployment_plan(
                &env,
                &version,
                DeploymentPlanSpec {
                    snapshot: None,
                    expected_secret_heads: Default::default(),
                    compare_secret_heads: true,
                    target_ids: Vec::new(),
                    proxy_ids: Vec::new(),
                    queue_fence: false,
                    operation_kind: "deployment".to_string(),
                    secret_change: None,
                    note: None,
                },
                request_owner,
                super::DEPLOYMENT_COORDINATOR_LEASE_SECONDS,
            )
            .await
            .unwrap();

        reconcile_plan(&h.state, plan.clone()).await.unwrap();
        let unchanged = h.state.store.deployment_plan(&plan.id).await.unwrap();
        assert_eq!(unchanged.state, "preparing");
        assert_eq!(unchanged.coordinator_id, Some(request_owner));

        h.state
            .store
            .release_deployment_plan_lease(&plan.id, request_owner)
            .await
            .unwrap();
        reconcile_plan(&h.state, plan).await.unwrap();
        let terminal = h.state.store.deployment_plan(&unchanged.id).await.unwrap();
        assert_eq!(terminal.state, "failed");
        assert_eq!(terminal.coordinator_id, None);
        assert_eq!(terminal.lease_expires_at, None);
    }

    #[tokio::test]
    async fn queue_leases_drain_before_the_production_pointer_is_committed() {
        use deis_queue::proto::queue::{
            AckRequest, MessageHandle, PullRequest, SendRequest, queue_client::QueueClient,
        };

        let h = harness_with_queue(true).await;
        let tenant = "demo.production";
        let mut queue = QueueClient::connect(h.queue_endpoint.clone().unwrap())
            .await
            .unwrap();
        for body in ["old lease", "next generation"] {
            let mut request = tonic::Request::new(SendRequest {
                queue_name: "jobs".to_string(),
                message_body: body.as_bytes().to_vec(),
            });
            request.metadata_mut().insert(
                deis_wire::conventions::TENANT_HEADER,
                tenant.parse().unwrap(),
            );
            queue.send(request).await.unwrap();
        }
        let mut request = tonic::Request::new(PullRequest {
            queue_name: "jobs".to_string(),
            max_messages: 1,
            lease_seconds: 60,
        });
        request.metadata_mut().insert(
            deis_wire::conventions::TENANT_HEADER,
            tenant.parse().unwrap(),
        );
        let old = queue.pull(request).await.unwrap().into_inner().messages;
        assert_eq!(old.len(), 1);

        json(
            &h.router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let bundle = bundle_with_manifest(
            r#"{"schema":1,"app":"demo","entry":"worker/index.js","queues":[{"name":"jobs"}]}"#,
            true,
        );
        let (_, uploaded) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(bundle),
            "application/zip",
        )
        .await;
        let version_id = uploaded["version"]["id"].as_str().unwrap();
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        let (status, accepted) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
        assert!(accepted.get("deployment").is_none(), "{accepted}");
        assert_eq!(accepted["plan"]["state"], "prepared");
        assert!(
            accepted["plan"].get("coordinator_id").is_none(),
            "the accepted response must reflect the request owner's explicit release: {accepted}"
        );
        let plan_id = accepted["plan"]["id"].as_str().unwrap();
        assert!(
            h.state
                .store
                .current_deployment(accepted["plan"]["env_id"].as_str().unwrap())
                .await
                .unwrap()
                .is_none()
        );
        let queue_observation = h
            .state
            .store
            .deployment_queue_observation(plan_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queue_observation.observed_state, "draining");
        assert_eq!(queue_observation.outstanding_leases, 1);
        assert_eq!(queue_observation.active_deployment_id, "");
        assert_eq!(
            queue_observation.pending_deployment_id.as_deref(),
            Some(plan_id)
        );

        let leased = &old[0];
        let mut ack = tonic::Request::new(AckRequest {
            queue_name: "jobs".to_string(),
            messages: vec![MessageHandle {
                id: leased.id.clone(),
                lease_token: leased.lease_token.clone(),
            }],
        });
        ack.metadata_mut().insert(
            deis_wire::conventions::TENANT_HEADER,
            tenant.parse().unwrap(),
        );
        queue.ack(ack).await.unwrap();

        let plan = h.state.store.deployment_plan(plan_id).await.unwrap();
        reconcile_plan(&h.state, plan).await.unwrap();
        let plan = h.state.store.deployment_plan(plan_id).await.unwrap();
        assert_eq!(plan.state, "activating");
        reconcile_plan(&h.state, plan).await.unwrap();
        let plan = h.state.store.deployment_plan(plan_id).await.unwrap();
        assert_eq!(plan.state, "active");
        let deployment = h
            .state
            .store
            .current_deployment(&plan.env_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deployment.id, plan.id);
        let queue_observation = h
            .state
            .store
            .deployment_queue_observation(plan_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queue_observation.observed_state, "active");
        assert_eq!(
            queue_observation.active_generation,
            plan.routing_generation.to_string()
        );
        assert_eq!(queue_observation.active_deployment_id, plan.id);
        assert!(queue_observation.pending_deployment_id.is_none());
        let (status, plan_status) = json(
            &h.router,
            "GET",
            &format!("/api/apps/demo/environments/production/deployment-plans/{plan_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{plan_status}");
        assert_eq!(plan_status["queue"]["active_deployment_id"], plan_id);
        assert!(plan_status["queue"].get("pending_deployment_id").is_none());
    }

    /// A committed plan whose only runtime target left the fleet used to hold
    /// its environment for ever.
    ///
    /// The mechanism, reproduced below: the plan's pointer has already moved,
    /// so it is past every state that can be aborted; reaching `active`
    /// requires every frozen target to report the exact generation; and a
    /// machine that is no longer on the fleet reports nothing, ever. Every
    /// state it can be in is in the set that refuses the next deployment to
    /// that environment, so the environment is undeployable and no operator
    /// action clears it - `DELETE .../registration` turns the push failure
    /// from one kind into another without making it less of a failure.
    ///
    /// What the test asserts, in order: the environment really is held; the
    /// deadline is not applied early; once it passes the plan finishes; the
    /// target is recorded as abandoned rather than as active, because nothing
    /// observed it running this version; and the environment takes a
    /// deployment again.
    #[tokio::test]
    async fn a_committed_plan_whose_target_left_the_fleet_stops_holding_its_environment() {
        let mut h = harness(true).await;
        json(
            &h.router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let (_, uploaded) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("demo")),
            "application/zip",
        )
        .await;
        let version_id = uploaded["version"]["id"].as_str().unwrap().to_string();
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;

        let env = h
            .state
            .store
            .get_environment("demo", "production")
            .await
            .unwrap();
        let version = h.state.store.get_version("demo", &version_id).await.unwrap();

        // A machine that was on the fleet when the plan was made and is not on
        // it now. Never registered here, which is the same thing this control
        // plane sees once a registration has expired: the name is in the
        // plan's frozen target list and nowhere else.
        let owner = CanonicalId::new_v7();
        let plan = h
            .state
            .store
            .create_deployment_plan(
                &env,
                &version,
                crate::catalog::store::DeploymentPlanSpec {
                    snapshot: None,
                    expected_secret_heads: Default::default(),
                    compare_secret_heads: false,
                    target_ids: vec!["runtime-gone".to_string()],
                    proxy_ids: Vec::new(),
                    queue_fence: false,
                    operation_kind: "deployment".to_string(),
                    secret_change: None,
                    note: None,
                },
                owner,
                30,
            )
            .await
            .unwrap();
        let prepared = h
            .state
            .store
            .transition_deployment_plan(&plan.id, "preparing", "prepared", None, owner)
            .await
            .unwrap();
        h.state
            .store
            .commit_deployment_plan(&prepared, &env, None, owner)
            .await
            .unwrap();
        assert_eq!(
            h.state.store.deployment_plan(&plan.id).await.unwrap().state,
            "activating",
            "the pointer has moved, so the plan is past anything that can be aborted"
        );

        // The symptom an operator meets: not a slow deployment, a refused one.
        let (status, refused) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "the held environment is the whole reason this matters: {refused}"
        );

        let lease =
            super::DeploymentPlanLease::start(h.state.store.clone(), plan.id.clone(), owner);

        // Before the deadline, nothing is given up on. A target that is merely
        // late must not be abandoned because a coordinator happened to look.
        let waited = super::finalize_if_observed(&h.state, &plan.id, &lease)
            .await
            .unwrap();
        assert_eq!(
            waited.state, "activating",
            "a plan seconds old must still be waiting for its target"
        );

        h.state.abandon_departed_target_after = std::time::Duration::ZERO;
        let finished = super::finalize_if_observed(&h.state, &plan.id, &lease)
            .await
            .unwrap();
        lease.finish().await;
        assert_eq!(
            finished.state, "active",
            "past the deadline, a target that is no longer a machine must not hold the plan"
        );
        assert!(
            finished
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("abandoned"),
            "the plan's own note has to say a target was given up on, or the only \
             record of it is a row nobody reads: {:?}",
            finished.last_error
        );

        let observations = h
            .state
            .store
            .deployment_target_observations(&plan.id)
            .await
            .unwrap();
        let observed = observations
            .iter()
            .find(|observation| observation.node_id == "runtime-gone")
            .expect("the abandoned target keeps a row of its own");
        assert_eq!(
            observed.observed_state, "abandoned",
            "nothing observed this machine running this version, so nothing may record that it did"
        );

        // And the point of all of it: the environment is deployable again.
        let (status, accepted) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;
        // 201 here rather than 202 because this one has a live target that
        // confirms inside the request; what is being asserted is that it was
        // taken at all, not which of the two successes it got.
        assert!(
            status.is_success(),
            "the environment must take a deployment once the held plan has ended, and it \
             answered {status}: {accepted}"
        );
        assert_eq!(
            accepted["plan"]["target_ids"],
            serde_json::json!(["runtime-test"]),
            "the new plan freezes the fleet as it is now, which no longer includes the machine \
             that left"
        );
    }

    /// The third way in, and the one that is easiest to dismiss: a plan that
    /// never committed anything.
    ///
    /// `aborting` is pre-commit compensation - the production pointer never
    /// moved - so it is tempting to think a stuck one is harmless. It is not:
    /// `aborting` is in the same set of states that refuses the next
    /// deployment to that environment, so a plan that changed nothing can hold
    /// the environment exactly as firmly as one that changed everything.
    #[tokio::test]
    async fn an_aborting_plan_whose_target_left_the_fleet_stops_holding_its_environment() {
        let mut h = harness(true).await;
        json(
            &h.router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let (_, uploaded) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("demo")),
            "application/zip",
        )
        .await;
        let version_id = uploaded["version"]["id"].as_str().unwrap().to_string();
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;

        let env = h
            .state
            .store
            .get_environment("demo", "production")
            .await
            .unwrap();
        let version = h.state.store.get_version("demo", &version_id).await.unwrap();
        let owner = CanonicalId::new_v7();
        let plan = h
            .state
            .store
            .create_deployment_plan(
                &env,
                &version,
                crate::catalog::store::DeploymentPlanSpec {
                    snapshot: None,
                    expected_secret_heads: Default::default(),
                    compare_secret_heads: false,
                    target_ids: vec!["runtime-gone".to_string()],
                    proxy_ids: Vec::new(),
                    queue_fence: false,
                    operation_kind: "deployment".to_string(),
                    secret_change: None,
                    note: None,
                },
                owner,
                30,
            )
            .await
            .unwrap();
        let aborting = h
            .state
            .store
            .transition_deployment_plan(
                &plan.id,
                "preparing",
                "aborting",
                Some("prepare did not reach every target"),
                owner,
            )
            .await
            .unwrap();
        h.state
            .store
            .release_deployment_plan_lease(&plan.id, owner)
            .await
            .unwrap();

        // Nothing changed and the environment is held anyway.
        let (status, refused) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a plan that committed nothing holds the environment just as hard: {refused}"
        );

        h.state.abandon_departed_target_after = std::time::Duration::ZERO;
        super::reconcile_plan(&h.state, aborting).await.unwrap();

        let finished = h.state.store.deployment_plan(&plan.id).await.unwrap();
        assert_eq!(
            finished.state, "failed",
            "the compensation has to reach a terminal state even when the machine it was aimed \
             at is gone"
        );
        let observations = h
            .state
            .store
            .deployment_target_observations(&plan.id)
            .await
            .unwrap();
        assert_eq!(
            observations
                .iter()
                .find(|observation| observation.node_id == "runtime-gone")
                .expect("the abandoned target keeps a row of its own")
                .observed_state,
            "abandoned",
            "nobody watched that machine drop the prepared version, so nothing may record that \
             it acknowledged the abort"
        );

        let (status, accepted) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;
        assert!(
            status.is_success(),
            "the environment must take a deployment again, and it answered {status}: {accepted}"
        );
    }

    /// The same hole on the other half of a plan's participants, and the
    /// reason it needed its own answer: a plan waiting on a proxy returns from
    /// the proxy gate in `reconcile_plan` and never reaches the finaliser, so
    /// an abandonment written only into the finaliser would have left this
    /// case exactly as it was.
    #[tokio::test]
    async fn a_committed_plan_whose_proxy_disconnected_stops_holding_its_environment() {
        let mut h = harness(true).await;
        let proxy = std::sync::Arc::new(crate::session::NodeSession::new(
            "proxy-gone".to_string(),
            crate::message::NodeInfo {
                node_role: crate::message::NodeRole::Proxy("test".to_string()),
                listen_host: "127.0.0.1".to_string(),
                listen_port: vec![8443],
            },
        ));
        proxy
            .apply(crate::session::NodeEvent::Registered)
            .await
            .unwrap();
        proxy
            .apply(crate::session::NodeEvent::Heartbeat)
            .await
            .unwrap();
        // Sessions are keyed by their own id, not by the node id they
        // announce, so the handle to take it away with is this one.
        let session_id = proxy.id.clone();
        h.sessions.add(proxy);

        json(
            &h.router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let (_, uploaded) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("demo")),
            "application/zip",
        )
        .await;
        let version_id = uploaded["version"]["id"].as_str().unwrap().to_string();
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;

        // The proxy is frozen into the plan because it is connected now.
        let (status, deployed) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{deployed}");
        assert_eq!(
            deployed["plan"]["proxy_ids"],
            serde_json::json!(["proxy-gone"])
        );
        let plan_id = deployed["plan"]["id"].as_str().unwrap().to_string();

        // And then it is gone, before it ever reported the table.
        assert!(
            h.sessions.remove(&session_id).is_some(),
            "the proxy has to actually leave, or this test asserts nothing"
        );

        // Through the entry point the resume loop uses, which claims the
        // plan's lease for itself - the deploy request that made the plan has
        // already released it.
        let plan = h.state.store.deployment_plan(&plan_id).await.unwrap();
        assert_eq!(
            plan.state, "activating",
            "the plan is waiting on a proxy that is not there any more"
        );
        h.state.abandon_departed_target_after = std::time::Duration::ZERO;
        super::reconcile_plan(&h.state, plan).await.unwrap();

        let finished = h.state.store.deployment_plan(&plan_id).await.unwrap();
        assert_eq!(
            finished.state, "active",
            "a proxy that is no longer connected must not hold the environment for ever"
        );
        let observations = h
            .state
            .store
            .deployment_proxy_observations(&plan_id)
            .await
            .unwrap();
        assert_eq!(
            observations
                .iter()
                .find(|observation| observation.proxy_id == "proxy-gone")
                .expect("the abandoned proxy keeps a row of its own")
                .observed_state,
            "abandoned",
            "nothing observed this proxy publishing this table, so nothing may record that it did"
        );
    }

    #[tokio::test]
    async fn active_waits_for_the_frozen_proxy_to_report_the_applied_table() {
        let h = harness(true).await;
        let proxy = std::sync::Arc::new(crate::session::NodeSession::new(
            "proxy-a".to_string(),
            crate::message::NodeInfo {
                node_role: crate::message::NodeRole::Proxy("test".to_string()),
                listen_host: "127.0.0.1".to_string(),
                listen_port: vec![8443],
            },
        ));
        proxy
            .apply(crate::session::NodeEvent::Registered)
            .await
            .unwrap();
        proxy
            .apply(crate::session::NodeEvent::Heartbeat)
            .await
            .unwrap();
        h.sessions.add(proxy);

        json(
            &h.router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let (_, uploaded) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("demo")),
            "application/zip",
        )
        .await;
        let version_id = uploaded["version"]["id"].as_str().unwrap();
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        let (status, deployed) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{deployed}");
        assert_eq!(deployed["plan"]["state"], "activating");
        assert_eq!(
            deployed["delivery"]["phase"], "prepare",
            "a proxy-delayed response must not present PREPARE acknowledgements as activation"
        );
        assert_eq!(
            deployed["plan"]["proxy_ids"],
            serde_json::json!(["proxy-a"])
        );
        let plan_id = deployed["plan"]["id"].as_str().unwrap().to_string();

        // The routing table used to be served over HTTP, with the node key
        // and a proxy id header standing in for who was asking; a proxy now
        // reaches it over the machine channel's mutual TLS instead, where
        // the peer's proved name is the proxy id and there is nothing left
        // to carry in a header. `routing_table_for` is what both the old
        // handler and the machine channel's RPC call underneath, so calling
        // it directly here tests the same acknowledgement bookkeeping
        // without standing up a second mTLS listener beside the one already
        // built for `RuntimeNodeManager` above.
        let table = routing_table::routing_table_for(&h.state, Some("proxy-a"), None)
            .await
            .expect("the routing table is built");
        let table_version = table.version.clone();

        routing_table::routing_table_for(
            &h.state,
            Some("proxy-a-impersonator"),
            Some(table_version.as_str()),
        )
        .await
        .expect("the routing table is built");
        let (_, not_yet_observed) = json(
            &h.router,
            "GET",
            &format!("/api/deployment-plans/{plan_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(not_yet_observed["plan"]["state"], "activating");

        routing_table::routing_table_for(&h.state, Some("proxy-a"), Some(table_version.as_str()))
            .await
            .expect("the routing table is built");
        let mut observed = Value::Null;
        let mut status = StatusCode::ACCEPTED;
        for _ in 0..100 {
            (status, observed) = json(
                &h.router,
                "GET",
                &format!("/api/deployment-plans/{plan_id}"),
                Value::Null,
            )
            .await;
            if observed["plan"]["state"] == "active" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(status, StatusCode::OK, "{observed}");
        assert_eq!(observed["plan"]["state"], "active");
        assert_eq!(observed["targets"][0]["observed_state"], "active");
        assert_eq!(observed["proxies"][0]["proxy_id"], "proxy-a");
        assert_eq!(
            observed["proxies"][0]["routing_table_version"],
            table_version
        );
    }

    /// An app with two versions where production currently runs v2, which
    /// is the only state in which a rollback has an unambiguous target.
    async fn deployed_twice() -> (Harness, String, String) {
        let h = harness(true).await;
        let router = &h.router;

        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let (_, body) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("demo")),
            "application/zip",
        )
        .await;
        let v1 = body["version"]["id"].as_str().unwrap().to_string();
        let (_, body) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(bundle_with_manifest(
                r#"{"schema":1,"app":"demo","entry":"worker/index.js","env":{"LOG_LEVEL":"debug"}}"#,
                true,
            )),
            "application/zip",
        )
        .await;
        let v2 = body["version"]["id"].as_str().unwrap().to_string();

        json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        for version in [&v1, &v2] {
            let (status, _) = json(
                router,
                "POST",
                "/api/apps/demo/environments/production/deployments",
                serde_json::json!({"version_id": version}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
        }

        (h, v1, v2)
    }

    async fn current_version(h: &Harness) -> String {
        let (status, body) = json(
            &h.router,
            "GET",
            "/api/apps/demo/environments/production/deployment",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        body["deployment"]["version_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn one_failed_prepare_keeps_the_production_pointer_unset() {
        let h = harness_with_failed_runtime(true).await;
        json(
            &h.router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let (_, uploaded) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("demo")),
            "application/zip",
        )
        .await;
        let version_id = uploaded["version"]["id"].as_str().unwrap();
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;

        let (status, refusal) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{refusal}");
        assert!(
            refusal
                .to_string()
                .contains("production pointer was not changed"),
            "{refusal}"
        );
        let (status, current) = json(
            &h.router,
            "GET",
            "/api/apps/demo/environments/production/deployment",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{current}");
    }

    /// Recording a deployment and pushing it to the nodes are one step
    /// from outside: the answer says where the version landed, so a caller
    /// never has to guess whether the fleet actually took it.
    #[tokio::test]
    async fn a_deployment_reports_where_it_was_delivered() {
        let (h, _v1, v2) = deployed_twice().await;

        let (status, body) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": v2}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["delivery"]["reason"], "deployment");
        assert_eq!(body["delivery"]["phase"], "activate");
        assert_eq!(body["delivery"]["version_id"], v2);
        assert_eq!(body["delivery"]["app_id"], "demo");
        assert_eq!(body["delivery"]["environment"], "production");
        assert!(body["delivery"]["skipped"].is_null());
        assert_eq!(
            body["delivery"]["delivered"],
            serde_json::json!(["runtime-test"])
        );
        assert_eq!(body["plan"]["state"], "active");
        assert_eq!(
            body["plan"]["target_ids"],
            serde_json::json!(["runtime-test"])
        );
        let plan_id = body["plan"]["id"].as_str().unwrap();
        let (status, scoped) = json(
            &h.router,
            "GET",
            &format!("/api/apps/demo/environments/production/deployment-plans/{plan_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{scoped}");
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "preview"}),
        )
        .await;
        let (status, hidden) = json(
            &h.router,
            "GET",
            &format!("/api/apps/demo/environments/preview/deployment-plans/{plan_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{hidden}");
    }

    #[tokio::test]
    async fn standard_deploy_and_secret_rotation_append_atomic_snapshot_pointers() {
        use deis_queue::proto::queue::{
            AckRequest, MessageHandle, PullRequest, SendRequest, queue_client::QueueClient,
        };

        let h = harness_with_queue(true).await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            vec![
                deis_wire::EnvironmentDeclaration {
                    name: "LABEL".to_string(),
                    kind: deis_wire::EnvironmentKind::Config,
                    value_type: deis_wire::EnvironmentValueType::String,
                    required: true,
                },
                deis_wire::EnvironmentDeclaration {
                    name: "TOKEN".to_string(),
                    kind: deis_wire::EnvironmentKind::Secret,
                    value_type: deis_wire::EnvironmentValueType::String,
                    required: true,
                },
            ],
        );
        json(
            &h.router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let manifest = serde_json::json!({
            "schema": 2,
            "app": "demo",
            "entry": "worker/index.js",
            "environment": declarations,
            "queues": [{"name": "jobs"}],
        })
        .to_string();
        let (status, uploaded) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(bundle_with_manifest(&manifest, true)),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{uploaded}");
        let version_id = uploaded["version"]["id"].as_str().unwrap();
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        let (status, secret) = json(
            &h.router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/TOKEN",
            serde_json::json!({"value": "first-value"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{secret}");

        let (status, deployed) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({
                "version_id": version_id,
                "config_values": {"LABEL": "edge"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{deployed}");
        let first_snapshot = deployed["deployment"]["snapshot_id"]
            .as_str()
            .unwrap()
            .to_string();
        let first_deployment = deployed["deployment"]["id"].as_str().unwrap().to_string();
        let first_generation = deployed["deployment"]["canonical_id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut queue = QueueClient::connect(h.queue_endpoint.clone().unwrap())
            .await
            .unwrap();
        let mut send = tonic::Request::new(SendRequest {
            queue_name: "jobs".to_string(),
            message_body: b"finish under the old secret".to_vec(),
        });
        send.metadata_mut().insert(
            deis_wire::conventions::TENANT_HEADER,
            "demo.production".parse().unwrap(),
        );
        queue.send(send).await.unwrap();
        let mut pull = tonic::Request::new(PullRequest {
            queue_name: "jobs".to_string(),
            max_messages: 1,
            lease_seconds: 60,
        });
        pull.metadata_mut().insert(
            deis_wire::conventions::TENANT_HEADER,
            "demo.production".parse().unwrap(),
        );
        pull.metadata_mut().insert(
            deis_wire::conventions::DEPLOYMENT_GENERATION_HEADER,
            first_generation.parse().unwrap(),
        );
        let old_lease = queue.pull(pull).await.unwrap().into_inner().messages;
        assert_eq!(old_lease.len(), 1);

        let (status, rotated) = json(
            &h.router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/TOKEN",
            serde_json::json!({"value": "second-value"}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{rotated}");
        assert_eq!(rotated["delivery"]["version_id"], version_id);
        let rotation_plan = rotated["plan"]["id"].as_str().unwrap();
        let staged_revision = rotated["secret"]["revision"].as_str().unwrap();
        assert_eq!(rotated["plan"]["state"], "prepared");
        assert_eq!(
            h.state
                .store
                .current_deployment(rotated["plan"]["env_id"].as_str().unwrap())
                .await
                .unwrap()
                .unwrap()
                .id,
            first_deployment,
            "a staged secret must not move the pointer before its queue barrier drains"
        );
        assert_ne!(
            h.state
                .store
                .current_secret_revision(rotated["plan"]["env_id"].as_str().unwrap(), "TOKEN")
                .await
                .unwrap()
                .as_deref(),
            Some(staged_revision),
            "202 exposes candidate metadata but the current secret head stays unchanged"
        );
        let leased = &old_lease[0];
        let mut ack = tonic::Request::new(AckRequest {
            queue_name: "jobs".to_string(),
            messages: vec![MessageHandle {
                id: leased.id.clone(),
                lease_token: leased.lease_token.clone(),
            }],
        });
        ack.metadata_mut().insert(
            deis_wire::conventions::TENANT_HEADER,
            "demo.production".parse().unwrap(),
        );
        queue.ack(ack).await.unwrap();
        let plan = h.state.store.deployment_plan(rotation_plan).await.unwrap();
        reconcile_plan(&h.state, plan).await.unwrap();
        let plan = h.state.store.deployment_plan(rotation_plan).await.unwrap();
        reconcile_plan(&h.state, plan).await.unwrap();
        assert_eq!(
            h.state
                .store
                .current_secret_revision(rotated["plan"]["env_id"].as_str().unwrap(), "TOKEN")
                .await
                .unwrap()
                .as_deref(),
            Some(staged_revision)
        );
        let (_, current) = json(
            &h.router,
            "GET",
            "/api/apps/demo/environments/production/deployment",
            Value::Null,
        )
        .await;
        let rotated_snapshot = current["deployment"]["snapshot_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(rotated_snapshot, first_snapshot);
        assert_eq!(current["deployment"]["version_id"], version_id);

        let (status, deleted) = json(
            &h.router,
            "DELETE",
            "/api/apps/demo/environments/production/secrets/TOKEN",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{deleted}");
        assert!(
            deleted["delivery"]["skipped"]
                .as_str()
                .unwrap()
                .contains("required secret")
        );
        let (_, still_current) = json(
            &h.router,
            "GET",
            "/api/apps/demo/environments/production/deployment",
            Value::Null,
        )
        .await;
        assert_eq!(
            still_current["deployment"]["snapshot_id"], rotated_snapshot,
            "deleting a required head changed the active retained snapshot"
        );

        let second_manifest = serde_json::json!({
            "schema": 2,
            "app": "demo",
            "entry": "worker/index.js",
            "environment": declarations,
            "routes": [{"pattern": "/second", "methods": []}],
        })
        .to_string();
        let (status, uploaded_second) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(bundle_with_manifest(&second_manifest, true)),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{uploaded_second}");
        let never_activated = uploaded_second["version"]["id"].as_str().unwrap();
        let (status, refused) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/rollback",
            serde_json::json!({"version_id": never_activated}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{refused}");
        assert!(
            refused
                .to_string()
                .contains("EC_ENV_ROLLBACK_SNAPSHOT_UNAVAILABLE")
        );
    }

    #[tokio::test]
    async fn historical_rollback_reactivates_its_snapshot_after_secret_heads_move() {
        let h = harness(true).await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            vec![deis_wire::EnvironmentDeclaration {
                name: "TOKEN".to_string(),
                kind: deis_wire::EnvironmentKind::Secret,
                value_type: deis_wire::EnvironmentValueType::String,
                required: true,
            }],
        );
        json(
            &h.router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        json(
            &h.router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/TOKEN",
            serde_json::json!({"value": "one"}),
        )
        .await;

        let manifest = |route: &str| {
            serde_json::json!({
                "schema": 2,
                "app": "demo",
                "entry": "worker/index.js",
                "environment": declarations,
                "routes": [{"pattern": route, "methods": []}],
            })
            .to_string()
        };
        let (_, first_upload) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(bundle_with_manifest(&manifest("/first"), true)),
            "application/zip",
        )
        .await;
        let first_version = first_upload["version"]["id"].as_str().unwrap().to_string();
        let (status, first_deploy) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": first_version}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{first_deploy}");

        let (status, first_rotation) = json(
            &h.router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/TOKEN",
            serde_json::json!({"value": "two"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first_rotation}");
        let (_, first_current) = json(
            &h.router,
            "GET",
            "/api/apps/demo/environments/production/deployment",
            Value::Null,
        )
        .await;
        let first_snapshot = first_current["deployment"]["snapshot_id"]
            .as_str()
            .unwrap()
            .to_string();

        let (_, second_upload) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(bundle_with_manifest(&manifest("/second"), true)),
            "application/zip",
        )
        .await;
        let second_version = second_upload["version"]["id"].as_str().unwrap();
        let (status, second_deploy) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": second_version}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{second_deploy}");
        let (status, third_head) = json(
            &h.router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/TOKEN",
            serde_json::json!({"value": "three"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{third_head}");

        let (status, rollback) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/rollback",
            serde_json::json!({"version_id": first_version}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{rollback}");
        assert_eq!(rollback["deployment"]["snapshot_id"], first_snapshot);
        assert_ne!(
            rollback["deployment"]["snapshot_id"], third_head["plan"]["snapshot_id"],
            "rollback must not silently rebind historical code to the latest secret heads"
        );
    }

    #[tokio::test]
    async fn a_rollback_is_delivered_too_and_says_it_was_a_rollback() {
        let (h, v1, _v2) = deployed_twice().await;

        let (status, body) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/rollback",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["delivery"]["reason"], "rollback");
        assert_eq!(body["delivery"]["version_id"], v1);
    }

    /// Reading the current pointer pushes nothing, so it must not pretend
    /// a delivery happened.
    #[tokio::test]
    async fn reading_the_current_deployment_carries_no_delivery() {
        let (h, _v1, _v2) = deployed_twice().await;

        let (status, body) = json(
            &h.router,
            "GET",
            "/api/apps/demo/environments/production/deployment",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("delivery").is_none(), "{body}");
    }

    /// A body that does not parse must be refused. Treating it as "no body
    /// given" would roll production back to the previous version without
    /// anyone asking for that.
    #[tokio::test]
    async fn a_malformed_rollback_body_is_rejected() {
        let (h, _v1, v2) = deployed_twice().await;

        for body in [
            // Right field, wrong type.
            r#"{"version_number": "3"}"#,
            // Not JSON at all.
            "{oops",
            // Valid JSON of the wrong shape.
            "[]",
        ] {
            let (status, response) = call(
                &h.router,
                "POST",
                "/api/apps/demo/environments/production/rollback",
                Body::from(body),
                "application/json",
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "body {body:?} must be refused, got {response}"
            );
            assert_eq!(
                current_version(&h).await,
                v2,
                "a refused rollback must leave the environment where it was"
            );
        }
    }

    /// The other half of the contract: no body at all still means "roll
    /// back to the previous version".
    #[tokio::test]
    async fn an_absent_rollback_body_still_rolls_back_to_the_previous_version() {
        let (h, v1, _v2) = deployed_twice().await;

        // Empty body with a JSON content type, which is what most clients
        // send when they pass no payload.
        let (status, body) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/rollback",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["deployment"]["version_id"], v1);

        // And with no content type header at all.
        let (h, v1, _v2) = deployed_twice().await;
        let request = Request::builder()
            .method("POST")
            .uri("/api/apps/demo/environments/production/rollback")
            .body(Body::empty())
            .unwrap();
        let response = h.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = to_bytes(response.into_body(), MAX_BUNDLE_BYTES)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["deployment"]["version_id"], v1);
    }
}
