//! Taking capabilities back.
//!
//! Every capability this control plane signs is in its issuance table (see
//! [`crate::capability_ledger`]), so an operator can revoke exactly the ones
//! that matter: one token by `jti`, every token a node holds, every token
//! that names one application environment - or, when the scope of a leak is
//! unknown, all of them at once by bumping the auth epoch.
//!
//! A revocation reaches the verifiers through the signed revocation list they
//! poll every 30 seconds, so it is in force within one poll.
//!
//! Revoking a jti or a tenant takes back tokens that exist; a node still in
//! the fleet and still assigned that tenant is issued a new one on its next
//! renewal, which is right for "that token leaked". Revoking a node is for
//! "that machine is compromised", and takes back the holder rather than its
//! tokens: the node is taken out of the fleet, the machine channel answers it
//! nothing (no registration, assignments, secrets or capabilities), and it
//! rides on the signed list by name, so every verifier refuses any capability
//! naming it - one issued a moment before the revocation, one read from its
//! offline cache, an expired one offered under T8 grace. That lasts until an
//! administrator lifts it. The platform cannot tell one certificate for a
//! name from another, so lift it only once the compromised key can no longer
//! prove that name: every certificate it held has expired, or the rebuilt
//! machine joins under a new node id.
//!
//! Every route here is an administrator's and is written to the audit log
//! whatever its outcome; see [`crate::api::authz`].

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Serialize;
use tracing::{info, warn};
use utoipa::ToSchema;

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};
use crate::api::openapi;
use crate::capability_ledger::RevocationScope;

/// What a revocation took back.
#[derive(Debug, Serialize, ToSchema)]
pub struct CapabilityRevocation {
    /// Every capability revoked by this call, by `jti`. Only capabilities
    /// that could still be accepted somewhere are listed; one that expired
    /// long ago needs no revoking.
    pub revoked: Vec<String>,
}

/// The auth epoch after a bump.
#[derive(Debug, Serialize, ToSchema)]
pub struct AuthEpoch {
    /// Every capability issued under a lower epoch is refused by every
    /// verifier once it adopts the next revocation list.
    pub auth_epoch: u64,
}

async fn revoke(state: &CatalogState, scope: RevocationScope) -> ApiResult<Vec<String>> {
    state
        .issuer
        .ledger()
        .revoke(scope)
        .await
        .map_err(|error| ApiError::DatabaseError(error.to_string()))
}

/// Revoke one capability.
///
/// Answers 404 when no capability with this `jti` was issued here, or it
/// expired long enough ago that nothing can accept it any more.
#[utoipa::path(
    delete,
    path = "/api/v1/capabilities/{jti}",
    operation_id = "revokeCapability",
    tag = "product",
    params(("jti" = String, Path, description = "The capability's `jti`.")),
    responses(
        (status = 200, description = "The capability is revoked; verifiers refuse it within one poll.", body = CapabilityRevocation),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn revoke_capability(
    State(state): State<CatalogState>,
    Path(jti): Path<String>,
) -> ApiResult<Json<CapabilityRevocation>> {
    let revoked = revoke(&state, RevocationScope::Jti(jti.clone())).await?;
    if revoked.is_empty() {
        return Err(ApiError::NotFound(format!(
            "no capability {jti} that could still be accepted was issued here"
        )));
    }
    info!(jti = %jti, "Revoked a capability; verifiers refuse it from their next poll");
    Ok(Json(CapabilityRevocation { revoked }))
}

/// Revoke a node.
///
/// Revokes every unexpired capability issued to the node with this id, and
/// the node itself until the revocation is lifted: it is taken out of the
/// fleet, refused every machine-channel call and named on the revocation
/// list, so no capability issued to it is accepted anywhere. Answers 200
/// with an empty list when it holds no capability; revoking a node already
/// revoked is not an error.
#[utoipa::path(
    post,
    path = "/api/v1/capabilities/revocations/nodes/{node_id}",
    operation_id = "revokeNode",
    tag = "product",
    params(("node_id" = String, Path, description = "The node's id, as its certificate names it.")),
    responses(
        (status = 200, description = "The node and every capability it held are revoked.", body = CapabilityRevocation),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn revoke_node(
    State(state): State<CatalogState>,
    Path(node_id): Path<String>,
) -> ApiResult<Json<CapabilityRevocation>> {
    let revoked = revoke(&state, RevocationScope::Node(node_id.clone())).await?;
    // Out of the fleet as well - a node named in `deis_runtime_nodes`
    // included: nothing is pushed to it, no cron fire is handed to it, no
    // proxy routes to it, and a registration racing this one cannot put it
    // back (see `RuntimeNodeManager::revoke`).
    let removed = state.nodes.revoke(&node_id);
    warn!(
        node = %node_id,
        revoked = revoked.len(),
        removed = ?removed,
        "Revoked a node: it is refused every machine call and every capability naming it until the revocation is lifted"
    );
    Ok(Json(CapabilityRevocation { revoked }))
}

/// Lift a node's revocation.
///
/// The node may register and be issued capabilities again. The capabilities
/// revoked with it stay revoked. Answers 404 when the node is not revoked.
#[utoipa::path(
    delete,
    path = "/api/v1/capabilities/revocations/nodes/{node_id}",
    operation_id = "liftNodeRevocation",
    tag = "product",
    params(("node_id" = String, Path, description = "The node's id, as its certificate names it.")),
    responses(
        (status = 204, description = "The node is no longer revoked; verifiers accept its new capabilities from their next poll."),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn lift_node_revocation(
    State(state): State<CatalogState>,
    Path(node_id): Path<String>,
) -> ApiResult<StatusCode> {
    let lifted = state
        .issuer
        .ledger()
        .lift_node(&node_id)
        .await
        .map_err(|error| ApiError::DatabaseError(error.to_string()))?;
    // The fleet is told whatever the ledger said: a revocation this process
    // held in memory but the ledger no longer does must not keep the node
    // out.
    let held = state.nodes.lift_revocation(&node_id);
    if !lifted && !held {
        return Err(ApiError::NotFound(format!("node {node_id} is not revoked")));
    }
    warn!(node = %node_id, "Lifted a node's revocation; it may register and be issued capabilities again");
    Ok(StatusCode::NO_CONTENT)
}

/// Revoke every capability for an environment's storage tenant.
///
/// Every unexpired capability that names this application environment,
/// whoever it was issued to - nodes, operators, the control plane itself.
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/environments/{env_name}/capability-revocations",
    operation_id = "revokeTenantCapabilities",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("env_name" = String, Path, description = "The environment's name.")),
    responses(
        (status = 200, description = "Every capability naming the tenant is revoked.", body = CapabilityRevocation),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn revoke_tenant_capabilities(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
) -> ApiResult<Json<CapabilityRevocation>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    // Store and queue capabilities name the canonical tenant, telemetry
    // capabilities the `<app>.<environment>` label its records carry; both
    // are this environment.
    let canonical = super::assignments::canonical_tenant_of(&state, &env.app_id, &env.id).await?;
    let label = deis_wire::conventions::tenant_id(&app_id, &env.name);
    let mut revoked = revoke(&state, RevocationScope::Tenant(canonical.clone())).await?;
    revoked.extend(revoke(&state, RevocationScope::Tenant(label.clone())).await?);
    info!(tenant = %canonical, label = %label, revoked = revoked.len(), "Revoked every capability naming a tenant");
    Ok(Json(CapabilityRevocation { revoked }))
}

/// Revoke every capability issued so far.
///
/// Bumps the auth epoch. Every capability issued before this call is refused
/// by every verifier once it adopts the next revocation list, whoever holds
/// it; nodes obtain new ones on their next renewal, and every other holder
/// has to ask again.
#[utoipa::path(
    post,
    path = "/api/v1/capabilities/auth-epoch",
    operation_id = "bumpAuthEpoch",
    tag = "product",
    responses(
        (status = 200, description = "The new auth epoch.", body = AuthEpoch),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn bump_auth_epoch(State(state): State<CatalogState>) -> ApiResult<Json<AuthEpoch>> {
    let auth_epoch = state
        .issuer
        .bump_auth_epoch()
        .await
        .map_err(|error| ApiError::DatabaseError(error.to_string()))?;
    warn!(
        auth_epoch,
        "Bumped the auth epoch: every capability issued before now is refused from the verifiers' next poll"
    );
    Ok(Json(AuthEpoch { auth_epoch }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Harness, harness_with_storage, json, storage_wiring};
    use axum::http::StatusCode;
    use deis_capability::{KeyRing, revocation};

    const MINT: &str = "/api/apps/demo/environments/production/storage-token";

    async fn platform() -> Harness {
        let h = harness_with_storage(true, storage_wiring()).await;
        json(&h.router, "POST", "/api/apps", serde_json::json!({"id": "demo"})).await;
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        h
    }

    fn jti(token: &str) -> String {
        let (payload, _) = deis_capability::parse_token(token).unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        claims["jti"].as_str().unwrap().to_string()
    }

    async fn published_list(h: &Harness) -> deis_capability::RevocationList {
        let keys = KeyRing::from_keys(h.state.issuer.published_keys().await);
        revocation::verify(&h.state.issuer.revocation_list().await.unwrap(), &keys).unwrap()
    }

    /// Each form of revocation reaches the signed list verifiers poll: one
    /// jti, every token naming a tenant, and the epoch for everything.
    #[tokio::test]
    async fn revocations_land_on_the_signed_list() {
        let h = platform().await;
        let (_, first) = json(&h.router, "GET", MINT, serde_json::Value::Null).await;
        let (_, second) = json(&h.router, "GET", MINT, serde_json::Value::Null).await;
        let one = jti(first["token"].as_str().unwrap());

        let (status, body) = json(
            &h.router,
            "DELETE",
            &format!("/api/capabilities/{one}"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["revoked"], serde_json::json!([one.clone()]));
        assert_eq!(published_list(&h).await.revoked_jtis, vec![one.clone()]);

        let (status, _) = json(
            &h.router,
            "DELETE",
            "/api/capabilities/no-such-jti",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/capability-revocations",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let listed = published_list(&h).await.revoked_jtis;
        for token in [&first["queue_token"], &second["token"], &second["queue_token"]] {
            assert!(listed.contains(&jti(token.as_str().unwrap())));
        }

        let (status, body) = json(
            &h.router,
            "POST",
            "/api/capabilities/auth-epoch",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["auth_epoch"], 2);
        assert_eq!(published_list(&h).await.auth_epoch, 2);
    }

    /// Revoking a node puts it on the signed list by name, so it outlasts
    /// the tokens it held; lifting it takes it off again, once.
    #[tokio::test]
    async fn revoking_a_node_lists_it_until_it_is_lifted() {
        let h = platform().await;
        let path = "/api/capabilities/revocations/nodes/runtime-test";
        let (status, body) = json(&h.router, "POST", path, serde_json::Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["revoked"], serde_json::json!([]));
        assert_eq!(published_list(&h).await.revoked_nodes, vec!["runtime-test"]);

        let (status, _) = json(&h.router, "DELETE", path, serde_json::Value::Null).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(published_list(&h).await.revoked_nodes.is_empty());
        let (status, _) = json(&h.router, "DELETE", path, serde_json::Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
