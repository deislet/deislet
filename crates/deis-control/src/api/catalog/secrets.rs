//! Secret endpoints - the human surface: write, delete, list names.
//!
//! Three rules shape this module. A value goes in encrypted and never comes
//! back out *here*: every read on this surface returns names and timestamps
//! only, so the dashboard and the CLI cannot read a secret back
//! (docs/architecture.zh.md section 8). When the master key is not
//! available the endpoints fail with 503 instead of falling back to
//! anything that would put a plaintext value in the database. And a change
//! here produces no new version - a secret is not part of the immutable
//! bundle - so it must redeliver the current version to the nodes by
//! itself, or the code would keep running with the value it was given at
//! deploy time.
//!
//! Where values *do* leave the process, so that this is not read as a
//! platform-wide claim it cannot make: the `ApplyDeployment` push and the
//! assignment list a node fetches, both of them decrypted at the moment of
//! sending and both authenticated as a node. See [`crate::delivery`].

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};
use crate::catalog::secrets::secret_revision_aad;
use crate::catalog::{DeploymentPlan, SecretMeta};
use crate::delivery::{DeliveryReport, SecretChangePreparation};

#[derive(Deserialize)]
pub struct PutSecretRequest {
    /// Plaintext value. It is encrypted before it touches the database and
    /// is never written to a log.
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct SecretsResponse {
    pub secrets: Vec<SecretMeta>,
    pub total: usize,
}

/// What a write to a secret answers: the metadata of the secret itself -
/// never its value - plus where the reloaded deployment landed.
#[derive(Debug, Serialize)]
pub struct SecretChangeResponse {
    pub secret: SecretMeta,
    /// Nodes that picked the new environment up. A secret change creates
    /// no version, so this is the only evidence it reached anything.
    pub delivery: DeliveryReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<DeploymentPlan>,
}

/// What deleting a secret answers.
#[derive(Debug, Serialize)]
pub struct SecretDeletedResponse {
    pub deleted: String,
    pub delivery: DeliveryReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<DeploymentPlan>,
}

/// PUT /api/apps/:app_id/environments/:env/secrets/:name
pub async fn put_secret(
    State(state): State<CatalogState>,
    Path((app_id, env_name, name)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(request): Json<PutSecretRequest>,
) -> ApiResult<Json<SecretChangeResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    deis_wire::validate_secret_value(&request.value)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;

    // Resolve the cipher before anything else so a missing master key can
    // never end with a plaintext value in the row.
    let cipher = state.secrets.cipher().map_err(|e| {
        warn!(
            app_id = %app_id,
            environment = %env_name,
            secret = %name,
            "Refused to store a secret because no usable master key is configured; the value was discarded rather than stored in the clear"
        );
        ApiError::from(e)
    })?;

    let expected_revision = state.store.current_secret_revision(&env.id, &name).await?;
    let revision = deis_wire::CanonicalId::new_v7().to_string();
    let ciphertext = cipher.encrypt(
        request.value.as_bytes(),
        &secret_revision_aad(&env.id, &name, &revision),
    )?;
    let secret = state
        .store
        .stage_secret_revision(&env, &name, &revision, ciphertext)
        .await?;

    // No new version exists to point at, so the running one is delivered
    // again with a freshly decrypted environment.
    let base = state.deliverer.base_url(&headers);
    let delivery_result = state
        .deliverer
        .prepare_secret_change(
            &env,
            &base,
            &name,
            Some(&secret.revision),
            expected_revision.as_deref(),
        )
        .await;
    let (delivery, plan) = match delivery_result {
        Ok(SecretChangePreparation::Finished(delivery)) => (delivery, None),
        Ok(SecretChangePreparation::Prepared {
            version,
            mut payload,
        }) => {
            let coordinated = super::deployments::coordinate_secret_change(
                &state,
                &env,
                &version,
                &mut payload,
                &name,
                Some(&secret.revision),
                expected_revision.as_deref(),
            )
            .await;
            let (_, plan, delivery) = match coordinated {
                Ok(result) => result,
                Err(error) => {
                    discard_staged_revision(&state, &env, &name, &secret.revision).await;
                    return Err(error);
                }
            };
            (delivery, Some(plan))
        }
        Err(error) => {
            discard_staged_revision(&state, &env, &name, &secret.revision).await;
            return Err(error.into());
        }
    };

    Ok(Json(SecretChangeResponse {
        secret,
        delivery,
        plan,
    }))
}

async fn discard_staged_revision(
    state: &CatalogState,
    env: &crate::catalog::Environment,
    name: &str,
    revision: &str,
) {
    match state
        .store
        .discard_secret_revision_if_unreferenced(&env.id, name, revision)
        .await
    {
        Ok(true) => warn!(
            app_id = %env.app_id,
            environment = %env.name,
            secret = %name,
            revision = %revision,
            code = "EC_ENV_STAGING_DISCARDED",
            "Secret activation failed; its unreferenced staged revision was removed"
        ),
        Ok(false) => warn!(
            app_id = %env.app_id,
            environment = %env.name,
            secret = %name,
            revision = %revision,
            code = "EC_ENV_STAGING_RETAINED",
            "Secret activation failed; cleanup retained a revision that is now referenced"
        ),
        Err(cleanup) => warn!(
            app_id = %env.app_id,
            environment = %env.name,
            secret = %name,
            revision = %revision,
            code = "EC_ENV_STAGING_CLEANUP_FAILED",
            error = %cleanup,
            "Secret activation failed and staged-revision cleanup must be retried"
        ),
    }
}

/// GET /api/apps/:app_id/environments/:env/secrets
///
/// Names and timestamps only. There is deliberately no endpoint that
/// returns a secret value.
pub async fn list_secrets(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
) -> ApiResult<Json<SecretsResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let secrets = state.store.list_secrets(&env.id).await?;
    Ok(Json(SecretsResponse {
        total: secrets.len(),
        secrets,
    }))
}

/// DELETE /api/apps/:app_id/environments/:env/secrets/:name
pub async fn delete_secret(
    State(state): State<CatalogState>,
    Path((app_id, env_name, name)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<SecretDeletedResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let expected_revision = state
        .store
        .current_secret_revision(&env.id, &name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("secret {name} in environment {}", env.id)))?;

    // Same reason as on write: without this the nodes would keep serving
    // the deleted value until something else moved the version pointer.
    let base = state.deliverer.base_url(&headers);
    let preparation = state
        .deliverer
        .prepare_secret_change(&env, &base, &name, None, Some(&expected_revision))
        .await?;
    let (delivery, plan) = match preparation {
        SecretChangePreparation::Finished(delivery) => (delivery, None),
        SecretChangePreparation::Prepared {
            version,
            mut payload,
        } => {
            let (_, plan, delivery) = super::deployments::coordinate_secret_change(
                &state,
                &env,
                &version,
                &mut payload,
                &name,
                None,
                Some(&expected_revision),
            )
            .await?;
            (delivery, Some(plan))
        }
    };

    Ok(Json(SecretDeletedResponse {
        deleted: name,
        delivery,
        plan,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{call, harness, json};

    use axum::body::Body;
    use axum::http::StatusCode;
    use serde_json::Value;

    /// Set up an app whose production environment runs one version.
    /// Returns the router harness and that version's id.
    async fn deployed() -> (super::super::tests::Harness, String) {
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
            Body::from(crate::catalog::bundle::tests::sample_bundle("demo")),
            "application/zip",
        )
        .await;
        let version_id = body["version"]["id"].as_str().unwrap().to_string();
        json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        json(
            router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;

        (h, version_id)
    }

    /// A secret change creates no version, so unless it redelivers the
    /// running one the nodes keep the value they were given at deploy
    /// time and the change quietly does nothing.
    #[tokio::test]
    async fn changing_a_secret_redelivers_the_running_version() {
        let (h, version_id) = deployed().await;

        let (status, body) = json(
            &h.router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/API_KEY",
            serde_json::json!({"value": "super-secret"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["secret"]["name"], "API_KEY");
        let first_revision = body["secret"]["revision"].as_str().unwrap().to_string();
        first_revision
            .parse::<deis_wire::CanonicalId>()
            .expect("secret writes expose an opaque UUIDv7 revision, not a value");
        assert_eq!(body["delivery"]["reason"], "secret_change");
        assert_eq!(
            body["delivery"]["version_id"], version_id,
            "the version that is running must be the one delivered again"
        );
        assert!(
            !body.to_string().contains("super-secret"),
            "neither the secret nor the delivery report may echo the value"
        );

        let (status, rotated) = json(
            &h.router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/API_KEY",
            serde_json::json!({"value": "rotated-secret"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rotated}");
        let second_revision = rotated["secret"]["revision"].as_str().unwrap();
        assert_ne!(second_revision, first_revision);
        assert!(!rotated.to_string().contains("rotated-secret"));
        let (status, listed) = json(
            &h.router,
            "GET",
            "/api/apps/demo/environments/production/secrets",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        assert_eq!(listed["secrets"][0]["revision"], second_revision);

        // Deleting is the same story: without a redelivery the nodes would
        // keep serving a value that no longer exists.
        let (status, body) = json(
            &h.router,
            "DELETE",
            "/api/apps/demo/environments/production/secrets/API_KEY",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], "API_KEY");
        assert_eq!(body["delivery"]["reason"], "secret_change");
        assert_eq!(body["delivery"]["version_id"], version_id);
    }

    /// Before a first deployment there is nothing to reload, and that is
    /// not an error: the value is picked up when the environment is
    /// deployed.
    #[tokio::test]
    async fn a_secret_set_before_any_deployment_has_nothing_to_reload() {
        let h = harness(true).await;
        let router = &h.router;

        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;

        let (status, body) = json(
            router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/API_KEY",
            serde_json::json!({"value": "super-secret"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["delivery"]["skipped"]
                .as_str()
                .unwrap()
                .contains("never been deployed")
        );
    }
}
