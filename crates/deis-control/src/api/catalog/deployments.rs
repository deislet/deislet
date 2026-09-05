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
use crate::catalog::{Deployment, Environment, Version};
use crate::delivery::{DeliveryReason, DeliveryReport};

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
    let expected_current = state.store.current_deployment(&env.id).await?;
    let payload = state
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

    let deployment = if let Some(snapshot) = payload.environment_snapshot() {
        let expected_secret_heads = payload.expected_secret_heads().ok_or_else(|| {
            ApiError::Conflict(
                "EC_ENV_SNAPSHOT_INVALID: standard deployment has no secret-head selection"
                    .to_string(),
            )
        })?;
        state
            .store
            .activate_deployment(
                &env,
                &version.id,
                snapshot,
                expected_current.as_ref().map(|row| row.id.as_str()),
                &expected_secret_heads,
                request.note,
            )
            .await?
    } else {
        state.store.deploy(&env, &version.id, request.note).await?
    };
    let delivery = state
        .deliverer
        .push(&payload, DeliveryReason::Deployment)
        .await;

    Ok((
        StatusCode::CREATED,
        Json(response(&env, deployment, &version, Some(delivery))),
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
    let expected_current = state.store.current_deployment(&env.id).await?;
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
    let payload = payload_result
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
    let deployment = if let Some(snapshot) = payload.environment_snapshot() {
        state
            .store
            .reactivate_deployment_snapshot(
                &env,
                &version.id,
                snapshot,
                expected_current.as_ref().map(|row| row.id.as_str()),
                Some(note),
            )
            .await?
    } else {
        state.store.deploy(&env, &version.id, Some(note)).await?
    };
    let delivery = state
        .deliverer
        .push(&payload, DeliveryReason::Rollback)
        .await;

    info!(
        app_id = %app_id,
        environment = %env_name,
        version_id = %version.id,
        number = version.number,
        nodes = delivery.delivered.len(),
        "Rollback recorded as a new deployment; the previous pointer stays in the history"
    );
    Ok((
        StatusCode::CREATED,
        Json(response(&env, deployment, &version, Some(delivery))),
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
    Ok(Json(response(&env, deployment, &version, None)))
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
) -> DeploymentResponse {
    DeploymentResponse {
        deployment,
        environment: env.name.clone(),
        version: VersionView::from(version),
        delivery,
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
    use super::super::tests::{Harness, call, harness, json};
    use crate::api::catalog::MAX_BUNDLE_BYTES;
    use crate::catalog::bundle::tests::{bundle_with_manifest, sample_bundle};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

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
        assert_eq!(body["delivery"]["version_id"], v2);
        assert_eq!(body["delivery"]["app_id"], "demo");
        assert_eq!(body["delivery"]["environment"], "production");
        // This harness configures no runtime nodes, so nothing was pushed
        // and the answer says so instead of implying success.
        assert!(
            body["delivery"]["skipped"]
                .as_str()
                .unwrap()
                .contains("no runtime nodes are configured")
        );
        assert_eq!(body["delivery"]["delivered"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn standard_deploy_and_secret_rotation_append_atomic_snapshot_pointers() {
        let h = harness(true).await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            "edge-canon.next@60eacb13cda28ecc2ecb5508a2eff95ef56dff66",
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
