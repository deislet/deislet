//! The capability token `deis kv`, `deis db` and `deis blob` act with.
//!
//! deis-store authorises a call through an Ed25519 capability token that
//! binds the tenant, audience, and expiration together. This endpoint mints
//! a short-lived (15 minute) capability for a human operator, which is the
//! same credential model nodes use via the machine channel.
//!
//! # Why this route has a verb of its own
//!
//! Minting here grants no authority a deploy credential could not reach on
//! its own: whoever can put code into an application can read and write that
//! application's data anyway, by deploying a handler that does it. What a
//! token changes is the blast radius of a leaked credential - it expires
//! and can be revoked by jti or auth epoch.
//!
//! So it answers to [`crate::identity::Verb::Storage`], which no other verb
//! implies, over a scope that has to name this exact application and this
//! exact environment: `storage:myapp/production` reaches it and
//! `storage:myapp/*` cannot be written down at all. A credential carrying
//! `deploy:*/*` is refused here. The one credential that still gets through
//! without naming the tenant is an unscoped one - what a first login mints -
//! because it is root by definition and is exactly what the scope model asks
//! an operator to retire.
//!
//! Every mint is recorded in the audit log whatever its outcome, because what
//! it hands back is a credential; see [`crate::api::authz`].

use deis_capability::{AUD_QUEUE, AUD_STORE, resources_for_audience};

use std::net::SocketAddr;

use axum::{
    Extension, Json,
    extract::{ConnectInfo, Path, State},
};
use serde::Serialize;
use tracing::{info, warn};
use utoipa::ToSchema;

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};
use crate::api::openapi;
use crate::identity::Principal;

/// What a caller needs in order to act as one tenant of deis-store.
///
/// Every field is always present: an answer carrying an empty token would be
/// one the caller cannot use, so that case is a refusal rather than a field
/// left blank.
#[derive(Debug, Serialize, ToSchema)]
pub struct StorageTokenResponse {
    /// The canonical tenant (`v1/<application id>/<environment id>`) the
    /// tokens name. deis-store and deis-queue act for exactly this tenant;
    /// it is returned so the tenant this control plane signed is visible to
    /// anyone debugging a refused call.
    pub tenant: String,
    /// Where deis-store listens, as configured on this control plane.
    pub store_endpoint: String,
    /// The Ed25519 capability token for deis-store (kv, sql and blob), for
    /// the `Authorization: Bearer` header.
    pub token: String,
    /// The Ed25519 capability token for deis-queue, for the same header on
    /// calls to deis-queue. A separate token because deis-queue accepts only
    /// its own audience.
    pub queue_token: String,
}

/// Mint a storage credential for an environment.
///
/// Answers 404 when the application or the environment is unknown - the same
/// resolution every other endpoint under this prefix does - and 409 when this
/// control plane is not configured to reach or authorise storage at all.
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/environments/{env_name}/storage-token",
    operation_id = "getStorageToken",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("env_name" = String, Path, description = "The environment's name.")),
    responses(
        (status = 200, description = "A credential for the environment's storage tenant.", body = StorageTokenResponse),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn storage_token(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
    peer: Option<ConnectInfo<SocketAddr>>,
    // Resolved by [`crate::api::authz::authorize`], which is the only thing
    // that mounts this route. It is taken here rather than re-derived from
    // the headers so that this line and the audit row name the same account
    // and the same credential; deriving it twice is how the two would drift.
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<StorageTokenResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let tenant = super::assignments::canonical_tenant_of(&state, &env.app_id, &env.id).await?;
    let caller = match peer {
        Some(ConnectInfo(addr)) => format!("{} from {addr}", principal.label()),
        None => format!("{} from an unknown address", principal.label()),
    };

    let Some(store_endpoint) = state.storage.store.clone() else {
        warn!(
            app_id = %app_id,
            environment = %env.name,
            %caller,
            "Refused a storage capability token because this control plane has no store_endpoint; there is nowhere for this tenant's kv, database or blob data to live, so no token would be usable"
        );
        return Err(ApiError::NotConfigured(format!(
            "This control plane has no deis-store configured, so {tenant} has nowhere to keep kv, \
             database or blob data. Set store_endpoint in the deis-control configuration file to \
             the address deis-store listens on, and restart it."
        )));
    };

    let subject = format!(
        "spiffe://deislet/human/{}",
        principal.account.as_deref().unwrap_or("unknown")
    );
    let issued = |aud: &'static str| {
        let issuer = state.issuer.clone();
        let subject = subject.clone();
        let tenant = tenant.clone();
        async move {
            issuer
                .issue(aud, &subject, resources_for_audience(&tenant, aud))
                .await
                .map_err(|error| {
                    ApiError::InternalError(format!("the {aud} capability could not be issued: {error}"))
                })
        }
    };
    let token = issued(AUD_STORE).await?;
    let queue_token = issued(AUD_QUEUE).await?;

    // The mint is also a row in the audit log, written by the gate that let
    // this call through (action `storage_token.mint`, recorded whatever the
    // outcome). This line is here as well because it is the one an operator
    // finds while reading the control plane's log for something else, and
    // what was handed out - credentials that talk to deis-store and
    // deis-queue directly - is worth meeting twice.
    info!(
        app_id = %app_id,
        environment = %env.name,
        tenant = %tenant,
        %caller,
        subject = %subject,
        store_endpoint = %store_endpoint,
        "Minted storage and queue capability tokens; for their 15 minutes the caller can read and write this tenant's kv, database and blob data directly against deis-store, and its queues against deis-queue"
    );

    Ok(Json(StorageTokenResponse {
        tenant,
        store_endpoint,
        token,
        queue_token,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        Harness, harness_with_storage, json, storage_wiring, storage_wiring_without_services,
    };

    use axum::http::StatusCode;

    const ROUTE: &str = "/api/apps/demo/environments/production/storage-token";

    /// An app with one environment, on a control plane wired the way the
    /// `storage` argument says.
    async fn platform(storage: crate::delivery::StorageWiring) -> Harness {
        let h = harness_with_storage(true, storage).await;
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
        h
    }

    /// The endpoint returns a valid Ed25519 capability token that deis-store
    /// can verify with the issuer's public key.
    #[tokio::test]
    async fn the_token_is_an_ed25519_capability() {
        let h = platform(storage_wiring()).await;
        let (status, body) = json(&h.router, "GET", ROUTE, serde_json::Value::Null).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let app = h.state.store.get_app("demo").await.unwrap();
        let env = h.state.store.get_environment("demo", "production").await.unwrap();
        let tenant = deis_wire::CanonicalTenantId::new(app.canonical_id, env.canonical_id).to_string();
        assert_eq!(body["tenant"], tenant.as_str());
        assert_eq!(body["store_endpoint"], "http://127.0.0.1:8090");

        let token = body["token"].as_str().unwrap();
        assert!(
            token.starts_with("v1."),
            "token must start with version prefix: {token}"
        );

        let verifying_key = h.state.issuer.verifying_key().await;
        let claims = deis_capability::decode(token, &verifying_key)
            .expect("token must be a valid Ed25519 capability");

        assert_eq!(claims.aud, "store", "aud must be 'store'");
        assert!(
            claims.authorizes_tenant(&tenant),
            "token must authorise the correct tenant"
        );
        // Human tokens expire in 15 minutes
        assert!(!claims.is_expired(), "freshly minted token must not be expired");

        // And each is accepted by the verifiers it is for: every store
        // service, and the queue.
        let verifier = deis_capability::CapabilityVerifier::new(verifying_key);
        for binding in deis_capability::STORE_BINDINGS {
            verifier
                .verify(token, "store", &tenant, binding)
                .unwrap_or_else(|e| panic!("store/{binding}: {e}"));
        }
        verifier
            .verify(
                body["queue_token"].as_str().unwrap(),
                "queue",
                &tenant,
                "queue",
            )
            .expect("the queue token is accepted by deis-queue");
    }

    /// A token minted for the wrong environment authorises a different
    /// tenant, so the two environments of one app must not share one.
    #[tokio::test]
    async fn each_environment_gets_its_own_token() {
        let h = platform(storage_wiring()).await;
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "preview"}),
        )
        .await;

        let (_, production) = json(&h.router, "GET", ROUTE, serde_json::Value::Null).await;
        let (status, preview) = json(
            &h.router,
            "GET",
            "/api/apps/demo/environments/preview/storage-token",
            serde_json::Value::Null,
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{preview}");
        assert_ne!(preview["tenant"], production["tenant"]);
        assert!(preview["tenant"].as_str().unwrap().starts_with("v1/"));
        assert_ne!(production["token"], preview["token"]);
    }

    #[tokio::test]
    async fn a_platform_with_no_store_says_which_key_to_set() {
        let h = platform(storage_wiring_without_services()).await;
        let (status, body) = json(&h.router, "GET", ROUTE, serde_json::Value::Null).await;

        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("store_endpoint"),
            "{body}"
        );
    }

    /// An environment that does not exist must not be mintable: the token is
    /// derived from the name, so answering would hand out a working
    /// credential for a tenant nobody deployed.
    #[tokio::test]
    async fn an_unknown_environment_is_not_minted_for() {
        let h = platform(storage_wiring()).await;
        let (status, _) = json(
            &h.router,
            "GET",
            "/api/apps/demo/environments/nope/storage-token",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = json(
            &h.router,
            "GET",
            "/api/apps/other/environments/production/storage-token",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
