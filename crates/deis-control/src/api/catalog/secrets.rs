//! Secret endpoints.
//!
//! Two rules shape this module. Values go in encrypted and never come back
//! out: reads return names and timestamps only. And when the master key is
//! not available the endpoints fail with 503 instead of falling back to
//! anything that would put a plaintext value in the database.

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};
use crate::catalog::SecretMeta;
use crate::catalog::secrets::secret_aad;

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

/// PUT /api/apps/:app_id/environments/:env/secrets/:name
pub async fn put_secret(
    State(state): State<CatalogState>,
    Path((app_id, env_name, name)): Path<(String, String, String)>,
    Json(request): Json<PutSecretRequest>,
) -> ApiResult<Json<SecretMeta>> {
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
    let meta = state.store.put_secret(&env, &name, ciphertext).await?;
    Ok(Json(meta))
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
) -> ApiResult<Json<serde_json::Value>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    state.store.delete_secret(&env, &name).await?;
    Ok(Json(serde_json::json!({ "deleted": name })))
}
