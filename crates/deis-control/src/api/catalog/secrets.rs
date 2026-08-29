//! Secret endpoints.
//!
//! Three rules shape this module. Values go in encrypted and never come
//! back out: reads return names and timestamps only. When the master key is
//! not available the endpoints fail with 503 instead of falling back to
//! anything that would put a plaintext value in the database. And a change
//! here produces no new version - a secret is not part of the immutable
//! bundle - so it must redeliver the current version to the nodes by
//! itself, or the code would keep running with the value it was given at
//! deploy time.

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};
use crate::catalog::SecretMeta;
use crate::catalog::secrets::secret_aad;
use crate::delivery::{DeliveryReason, DeliveryReport};

#[derive(Debug, Deserialize)]
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
}

/// What deleting a secret answers.
#[derive(Debug, Serialize)]
pub struct SecretDeletedResponse {
    pub deleted: String,
    pub delivery: DeliveryReport,
}

/// PUT /api/apps/:app_id/environments/:env/secrets/:name
pub async fn put_secret(
    State(state): State<CatalogState>,
    Path((app_id, env_name, name)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(request): Json<PutSecretRequest>,
) -> ApiResult<Json<SecretChangeResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;

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

    let ciphertext = cipher.encrypt(request.value.as_bytes(), &secret_aad(&env.id, &name))?;
    let secret = state.store.put_secret(&env, &name, ciphertext).await?;

    // No new version exists to point at, so the running one is delivered
    // again with a freshly decrypted environment.
    let base = state.deliverer.base_url(&headers);
    let delivery = state
        .deliverer
        .reload_current(&env, &base, DeliveryReason::SecretChange)
        .await;

    Ok(Json(SecretChangeResponse { secret, delivery }))
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
    state.store.delete_secret(&env, &name).await?;

    // Same reason as on write: without this the nodes would keep serving
    // the deleted value until something else moved the version pointer.
    let base = state.deliverer.base_url(&headers);
    let delivery = state
        .deliverer
        .reload_current(&env, &base, DeliveryReason::SecretChange)
        .await;

    Ok(Json(SecretDeletedResponse {
        deleted: name,
        delivery,
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
        assert_eq!(body["delivery"]["reason"], "secret_change");
        assert_eq!(
            body["delivery"]["version_id"], version_id,
            "the version that is running must be the one delivered again"
        );
        assert!(
            !body.to_string().contains("super-secret"),
            "neither the secret nor the delivery report may echo the value"
        );

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
