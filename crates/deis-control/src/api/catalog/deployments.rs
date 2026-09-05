//! Deployment endpoints.
//!
//! A deployment is a pointer row, so every operation here is an insert:
//! deploying moves an environment forward, rolling back moves it
//! backwards, and both leave the full history behind them.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tracing::{info, warn};

use super::CatalogState;
use super::versions::VersionView;
use crate::api::error::{ApiError, ApiResult};
use crate::catalog::{
    Deployment, DeploymentDrainObservation, DeploymentEvent, DeploymentGenerationDrain,
    DeploymentPlan, DeploymentPlanSpec, DeploymentProxyObservation, DeploymentTargetObservation,
    Environment, Version, now_secs,
};
use crate::delivery::{DeliveryReason, DeliveryReport, DeploymentPayload};
use crate::message::NodeRole;

/// Default and maximum number of history rows returned at once.
const DEFAULT_HISTORY_LIMIT: usize = 50;
const MAX_HISTORY_LIMIT: usize = 500;

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize, Default)]
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

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct DeploymentResponse {
    pub deployment: Deployment,
    pub environment: String,
    pub version: VersionView,
    /// Which nodes took this version. Absent when the response only reads
    /// back an existing pointer, because then nothing was pushed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<DeploymentPlan>,
}

#[derive(Debug, Serialize)]
pub struct DeploymentPlanResponse {
    pub plan: DeploymentPlan,
    pub targets: Vec<DeploymentTargetObservation>,
    pub proxies: Vec<DeploymentProxyObservation>,
    pub events: Vec<DeploymentEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drain: Option<DeploymentGenerationDrain>,
    pub drain_observations: Vec<DeploymentDrainObservation>,
}

#[derive(Debug, Serialize)]
pub struct DeploymentHistoryResponse {
    pub deployments: Vec<Deployment>,
    pub total: usize,
}

/// POST /api/apps/:app_id/environments/:env/deployments
///
/// Three steps, in this order: build what the nodes will be sent, record
/// the pointer, push it. Building first is the point of the ordering - an
/// environment whose secrets cannot be decrypted is refused outright,
/// rather than recorded as deployed and then delivered with half its
/// environment missing.
pub async fn deploy(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<DeployRequest>,
) -> ApiResult<(StatusCode, Json<DeploymentResponse>)> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let version = resolve_version(
        &state,
        &app_id,
        request.version_id.as_deref(),
        request.version_number,
    )
    .await?;

    let base = state.deliverer.base_url(&headers);
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
        Json(response(
            &env,
            deployment,
            &version,
            Some(delivery),
            Some(plan),
        )),
    ))
}

/// POST /api/apps/:app_id/environments/:env/rollback
///
/// Inserts a pointer row aimed at an older version. Nothing is mutated and
/// nothing is deleted, so the rollback itself is just as reversible.
pub async fn rollback(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<DeploymentResponse>)> {
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

    let base = state.deliverer.base_url(&headers);
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
        Json(response(
            &env,
            deployment,
            &version,
            Some(delivery),
            Some(plan),
        )),
    ))
}

/// GET /api/apps/:app_id/environments/:env/deployment
///
/// The current version of an environment: the newest pointer row.
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
    Ok(Json(response(&env, deployment, &version, None, plan)))
}

/// GET /api/deployment-plans/:id
pub async fn deployment_plan(
    State(state): State<CatalogState>,
    Path(id): Path<String>,
) -> ApiResult<Json<DeploymentPlanResponse>> {
    Ok(Json(deployment_plan_response(&state, &id).await?))
}

/// Scoped form used by deployment credentials. The path's environment is
/// resolved and compared with the plan, so a caller cannot put an authorised
/// prefix around another environment's opaque plan id.
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
    let events = state.store.deployment_events(id).await?;
    let drain = state.store.deployment_generation_drain(id).await?;
    let drain_observations = state.store.deployment_drain_observations(id).await?;
    Ok(DeploymentPlanResponse {
        plan,
        targets,
        proxies,
        events,
        drain,
        drain_observations,
    })
}

/// GET /api/apps/:app_id/environments/:env/deployments
pub async fn list_deployments(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
    Query(query): Query<HistoryQuery>,
) -> ApiResult<Json<DeploymentHistoryResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);
    let deployments = state.store.list_deployments(&env.id, limit).await?;
    Ok(Json(DeploymentHistoryResponse {
        total: deployments.len(),
        deployments,
    }))
}

async fn coordinate_strict_deployment(
    state: &CatalogState,
    env: &Environment,
    version: &Version,
    payload: &mut DeploymentPayload,
    note: Option<String>,
    reason: DeliveryReason,
) -> ApiResult<(Deployment, DeploymentPlan, DeliveryReport)> {
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
) -> ApiResult<(Deployment, DeploymentPlan, DeliveryReport)> {
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
) -> ApiResult<(Deployment, DeploymentPlan, DeliveryReport)> {
    let expected_secret_heads = payload.expected_secret_heads().unwrap_or_default();
    let snapshot_id = payload
        .environment_snapshot()
        .map(|snapshot| snapshot.identity())
        .transpose()
        .map_err(|error| ApiError::Conflict(error.to_string()))?;
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

    let plan = state
        .store
        .create_deployment_plan(
            env,
            version,
            DeploymentPlanSpec {
                snapshot_id,
                expected_secret_heads,
                compare_secret_heads: reason != DeliveryReason::Rollback && secret_change.is_none(),
                target_ids,
                proxy_ids,
                queue_fence,
                operation_kind: operation_kind.to_string(),
                secret_change,
                note,
            },
        )
        .await?;
    payload.set_instruction(
        &plan.id,
        plan.routing_generation.to_string(),
        crate::runtime_client::proto::DeploymentAction::Prepare,
    );
    let prepared = state
        .deliverer
        .push_to(payload, reason, &plan.target_ids)
        .await;
    record_target_report(state, &plan, &prepared, "prepared").await?;
    if !prepared.fully_delivered() {
        payload.set_instruction(
            &plan.id,
            plan.routing_generation.to_string(),
            crate::runtime_client::proto::DeploymentAction::Abort,
        );
        if !prepared.delivered.is_empty() {
            let abort = state
                .deliverer
                .push_to(payload, reason, &prepared.delivered)
                .await;
            record_target_report(state, &plan, &abort, "aborted").await?;
        }
        let detail = delivery_failure_detail(&prepared);
        state
            .store
            .transition_deployment_plan(&plan.id, "preparing", "failed", Some(&detail))
            .await?;
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
        )
        .await?;
    let deployment = match state
        .store
        .commit_deployment_plan(&plan, env, payload.environment_snapshot())
        .await
    {
        Ok(deployment) => deployment,
        Err(error) => {
            payload.set_instruction(
                &plan.id,
                plan.routing_generation.to_string(),
                crate::runtime_client::proto::DeploymentAction::Abort,
            );
            let abort = state
                .deliverer
                .push_to(payload, reason, &plan.target_ids)
                .await;
            record_target_report(state, &plan, &abort, "aborted").await?;
            let detail = error.to_string();
            state
                .store
                .transition_deployment_plan(&plan.id, "prepared", "failed", Some(&detail))
                .await?;
            return Err(error.into());
        }
    };

    // A prepared generation already serves exact HTTP requests. Keep node
    // current selectors and asynchronous triggers on the baseline until every
    // frozen proxy proves the new routing generation is locally published.
    if !plan.proxy_ids.is_empty() {
        return Ok((
            deployment,
            state.store.deployment_plan(&plan.id).await?,
            prepared,
        ));
    }

    let activating_plan = state.store.deployment_plan(&plan.id).await?;
    if !advance_queue_fence(state, env, &activating_plan).await? {
        return Ok((
            deployment,
            state.store.deployment_plan(&plan.id).await?,
            prepared,
        ));
    }

    payload.set_instruction(
        &plan.id,
        plan.routing_generation.to_string(),
        crate::runtime_client::proto::DeploymentAction::Activate,
    );
    let activated = state
        .deliverer
        .push_to(payload, reason, &plan.target_ids)
        .await;
    record_target_report(state, &plan, &activated, "active").await?;
    if !activated.fully_delivered() {
        let detail = delivery_failure_detail(&activated);
        state
            .store
            .transition_deployment_plan(&plan.id, "activating", "reconciling", Some(&detail))
            .await?;
    }
    let plan = finalize_if_observed(state, &plan.id).await?;
    Ok((deployment, plan, activated))
}

async fn record_target_report(
    state: &CatalogState,
    plan: &DeploymentPlan,
    report: &DeliveryReport,
    delivered_state: &str,
) -> ApiResult<()> {
    for node_id in &report.delivered {
        state
            .store
            .record_deployment_target_observation(DeploymentTargetObservation {
                plan_id: plan.id.clone(),
                node_id: node_id.clone(),
                observed_state: delivered_state.to_string(),
                version_id: Some(plan.version_id.clone()),
                routing_generation: Some(plan.routing_generation.to_string()),
                message: None,
                observed_at: now_secs(),
            })
            .await?;
    }
    for failure in &report.failed {
        state
            .store
            .record_deployment_target_observation(DeploymentTargetObservation {
                plan_id: plan.id.clone(),
                node_id: failure.node.clone(),
                observed_state: "unconfirmed".to_string(),
                version_id: Some(plan.version_id.clone()),
                routing_generation: Some(plan.routing_generation.to_string()),
                message: Some(failure.error.clone()),
                observed_at: now_secs(),
            })
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
pub(super) async fn finalize_if_observed(
    state: &CatalogState,
    plan_id: &str,
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
    let targets = state.store.deployment_target_observations(plan_id).await?;
    let proxies = state.store.deployment_proxy_observations(plan_id).await?;
    let generation = plan.routing_generation.to_string();
    let all_targets_active = plan.target_ids.iter().all(|target| {
        targets.iter().any(|observation| {
            observation.node_id == *target
                && observation.observed_state == "active"
                && observation.version_id.as_deref() == Some(plan.version_id.as_str())
                && observation.routing_generation.as_deref() == Some(generation.as_str())
        })
    });
    let all_proxies_applied = plan.proxy_ids.iter().all(|proxy| {
        proxies.iter().any(|observation| {
            observation.proxy_id == *proxy
                && observation.observed_state == "routing_applied"
                && observation.routing_generation == generation
        })
    });
    if all_targets_active && all_proxies_applied {
        return Ok(state
            .store
            .transition_deployment_plan(
                &plan.id,
                &plan.state,
                "active",
                Some("all frozen runtime and proxy targets report the exact production generation"),
            )
            .await?);
    }
    Ok(plan)
}

/// Resume every plan whose outcome was not durably terminal. All wire actions
/// are idempotent and carry the exact plan/generation, so replay is safer than
/// guessing which side of a lost response completed.
pub(crate) fn resume_on_start(state: CatalogState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
    let plan = state.store.deployment_plan(&drain.plan_id).await?;
    if plan.state != "active" {
        return Ok(());
    }
    let env = state.store.get_environment_by_id(&plan.env_id).await?;
    let version = state
        .store
        .get_version(&plan.app_id, &plan.version_id)
        .await?;
    let base_url = state.deliverer.reconciliation_base_url();
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
        .set_deployment_generation_drain_state(&plan.id, "draining", None)
        .await?;
    let report = state
        .deliverer
        .push_to(&payload, plan_delivery_reason(&plan)?, &drain.target_ids)
        .await;
    for node_id in &report.delivered {
        state
            .store
            .record_deployment_drain_observation(DeploymentDrainObservation {
                plan_id: plan.id.clone(),
                node_id: node_id.clone(),
                generation: drain.generation.clone(),
                observed_state: "drained".to_string(),
                message: None,
                observed_at: now_secs(),
            })
            .await?;
    }
    for failure in &report.failed {
        state
            .store
            .record_deployment_drain_observation(DeploymentDrainObservation {
                plan_id: plan.id.clone(),
                node_id: failure.node.clone(),
                generation: drain.generation.clone(),
                observed_state: "draining".to_string(),
                message: Some(failure.error.clone()),
                observed_at: now_secs(),
            })
            .await?;
    }
    if report.fully_delivered() {
        state
            .store
            .set_deployment_generation_drain_state(&plan.id, "drained", None)
            .await?;
    } else {
        state
            .store
            .set_deployment_generation_drain_state(
                &plan.id,
                "draining",
                Some(&delivery_failure_detail(&report)),
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
    if plan.target_ids.is_empty() {
        state
            .store
            .transition_deployment_plan(
                &plan.id,
                &plan.state,
                "failed",
                Some("strict deployment plan has no runtime serving target"),
            )
            .await?;
        return Ok(());
    }
    let env = state.store.get_environment_by_id(&plan.env_id).await?;
    let version = state
        .store
        .get_version(&plan.app_id, &plan.version_id)
        .await?;
    let base_url = state.deliverer.reconciliation_base_url();
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
            let report = state
                .deliverer
                .push_to(&payload, reason, &plan.target_ids)
                .await;
            record_target_report(state, &plan, &report, "prepared").await?;
            if report.fully_delivered() {
                state
                    .store
                    .transition_deployment_plan(
                        &plan.id,
                        "preparing",
                        "prepared",
                        Some("restart reconciliation confirmed every prepared target"),
                    )
                    .await?;
            }
        }
        "prepared" => {
            match state
                .store
                .commit_deployment_plan(&plan, &env, payload.environment_snapshot())
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    payload.set_instruction(
                        &plan.id,
                        plan.routing_generation.to_string(),
                        crate::runtime_client::proto::DeploymentAction::Abort,
                    );
                    let report = state
                        .deliverer
                        .push_to(&payload, reason, &plan.target_ids)
                        .await;
                    record_target_report(state, &plan, &report, "aborted").await?;
                    state
                        .store
                        .transition_deployment_plan(
                            &plan.id,
                            "prepared",
                            "failed",
                            Some(&error.to_string()),
                        )
                        .await?;
                }
            }
        }
        "activating" | "reconciling" | "degraded" => {
            if !all_proxies_observed(state, &plan).await? {
                return Ok(());
            }
            if !advance_queue_fence(state, &env, &plan).await? {
                return Ok(());
            }
            payload.set_instruction(
                &plan.id,
                plan.routing_generation.to_string(),
                crate::runtime_client::proto::DeploymentAction::Activate,
            );
            let report = state
                .deliverer
                .push_to(&payload, reason, &plan.target_ids)
                .await;
            record_target_report(state, &plan, &report, "active").await?;
            if !report.fully_delivered() && plan.state == "activating" {
                state
                    .store
                    .transition_deployment_plan(
                        &plan.id,
                        "activating",
                        "reconciling",
                        Some(&delivery_failure_detail(&report)),
                    )
                    .await?;
            }
            finalize_if_observed(state, &plan.id).await?;
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

/// Advance the durable queue lease barrier. Transport failures after the
/// pointer commit are reconciliable state, not a false rollback response: the
/// plan remains non-terminal and the periodic worker retries it.
async fn advance_queue_fence(
    state: &CatalogState,
    env: &Environment,
    plan: &DeploymentPlan,
) -> ApiResult<bool> {
    if !plan_requires_queue_fence(plan)? {
        return Ok(true);
    }
    match state
        .deliverer
        .activate_consumer_generation(env, plan)
        .await
    {
        Ok(progress) if progress.activated => {
            info!(
                deployment_plan = %plan.id,
                routing_generation = %plan.routing_generation,
                "Queue lease admission moved to the deployment generation"
            );
            Ok(true)
        }
        Ok(progress) => {
            info!(
                deployment_plan = %plan.id,
                routing_generation = %plan.routing_generation,
                outstanding_leases = progress.outstanding_leases,
                "Queue admission is closed while the previous generation's leases drain"
            );
            Ok(false)
        }
        Err(error) => {
            warn!(
                deployment_plan = %plan.id,
                routing_generation = %plan.routing_generation,
                error = %error,
                "Queue generation fence did not converge; runtime activation remains blocked and reconciliation will retry"
            );
            if plan.state == "activating" {
                state
                    .store
                    .transition_deployment_plan(
                        &plan.id,
                        "activating",
                        "reconciling",
                        Some(&format!("queue generation fence: {error}")),
                    )
                    .await?;
            }
            Ok(false)
        }
    }
}

async fn all_proxies_observed(state: &CatalogState, plan: &DeploymentPlan) -> ApiResult<bool> {
    let observations = state.store.deployment_proxy_observations(&plan.id).await?;
    let generation = plan.routing_generation.to_string();
    Ok(plan.proxy_ids.iter().all(|proxy| {
        observations.iter().any(|observation| {
            observation.proxy_id == *proxy
                && observation.observed_state == "routing_applied"
                && observation.routing_generation == generation
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
    deployment: Deployment,
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
    use super::super::tests::{Harness, call, harness, harness_with_failed_runtime, json};
    use crate::api::catalog::MAX_BUNDLE_BYTES;
    use crate::catalog::bundle::tests::{bundle_with_manifest, sample_bundle};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

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

        let first = Request::builder()
            .method("GET")
            .uri(deis_wire::conventions::ROUTING_TABLE_ROUTE)
            .header("authorization", "Bearer node-psk")
            .header(deis_wire::conventions::PROXY_ID_HEADER, "proxy-a")
            .body(Body::empty())
            .unwrap();
        let response = h.router.clone().oneshot(first).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let table: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), MAX_BUNDLE_BYTES)
                .await
                .unwrap(),
        )
        .unwrap();
        let table_version = table["version"].as_str().unwrap();

        let forged = Request::builder()
            .method("GET")
            .uri(deis_wire::conventions::ROUTING_TABLE_ROUTE)
            .header("authorization", "Bearer node-psk")
            .header(
                deis_wire::conventions::PROXY_ID_HEADER,
                "proxy-a-impersonator",
            )
            .header(
                deis_wire::conventions::OBSERVED_ROUTING_TABLE_HEADER,
                table_version,
            )
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            h.router.clone().oneshot(forged).await.unwrap().status(),
            StatusCode::OK
        );
        let (_, not_yet_observed) = json(
            &h.router,
            "GET",
            &format!("/api/deployment-plans/{plan_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(not_yet_observed["plan"]["state"], "activating");

        let ack = Request::builder()
            .method("GET")
            .uri(deis_wire::conventions::ROUTING_TABLE_ROUTE)
            .header("authorization", "Bearer node-psk")
            .header(deis_wire::conventions::PROXY_ID_HEADER, "proxy-a")
            .header(
                deis_wire::conventions::OBSERVED_ROUTING_TABLE_HEADER,
                table_version,
            )
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            h.router.clone().oneshot(ack).await.unwrap().status(),
            StatusCode::OK
        );
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
        let h = harness(true).await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            "edge-canon.next@c926687f7539453ab31776cd16f8341133d82a13",
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

        let (status, rotated) = json(
            &h.router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/TOKEN",
            serde_json::json!({"value": "second-value"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rotated}");
        assert_eq!(rotated["delivery"]["version_id"], version_id);
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
            "edge-canon.next@c926687f7539453ab31776cd16f8341133d82a13",
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
