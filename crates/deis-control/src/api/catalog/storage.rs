//! The capability token `deis kv`, `deis db` and `deis blob` act with.
//!
//! deis-store authorises a call by the tenant it names and the token it
//! presents, and the token is an HMAC of that tenant under a secret only the
//! control plane and the storage services hold (`storage_auth_secret`, see
//! [`crate::delivery::StorageWiring`]). A deployment already carries one, so
//! an application can reach its own data; this endpoint hands the same token
//! to a person, which is what makes the three CLI commands usable at all.
//!
//! # Why this route has a verb of its own
//!
//! Minting here grants no authority a deploy credential could not reach on
//! its own: whoever can put code into an application can read and write that
//! application's data anyway, by deploying a handler that does it. What a
//! token changes is the blast radius of a leaked credential - it is usable
//! directly against deis-store, without passing through this process again,
//! and it does not expire.
//!
//! So it answers to [`crate::identity::Verb::Storage`], which no other verb
//! implies, over a scope that has to name this exact application and this
//! exact environment: `storage:myapp/production` reaches it and
//! `storage:myapp/*` cannot be written down at all. A credential carrying
//! `deploy:*/*` is refused here. The one credential that still gets through
//! without naming the tenant is an unscoped one - a configuration-file
//! `api_keys` entry, or the bootstrap token - because those are root by
//! definition and are exactly what the scope model asks an operator to
//! retire.
//!
//! Every mint is recorded in the audit log whatever its outcome, because what
//! it hands back is a credential; see [`crate::api::authz`].
//!
//! # Why a missing secret is refused rather than answered with nothing
//!
//! `StorageWiring::token_for` answers `None` on a platform that mints no
//! tokens, and an all-loopback deis-store with no `auth_secret` of its own
//! would accept the tenant header alone. But this process cannot see the
//! store's configuration, and the two are only ever set together (the demo
//! sets all three). Answering with an empty token would therefore turn a
//! control-plane misconfiguration into an `Unauthenticated` from a different
//! process, which is where the last three people looked first. The refusal
//! below names the key to set instead.

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
    /// The `<app>.<environment>` pair the token authorises, which the caller
    /// must send back in `x-deis-tenant` on every storage call. Included so
    /// a caller never has to rebuild it, and so the one this control plane
    /// signed is visible to anyone debugging a refused call.
    pub tenant: String,
    /// Where deis-store listens, as configured on this control plane.
    pub store_endpoint: String,
    /// The capability token, for `x-deis-storage-token`.
    pub token: String,
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
    let tenant = deis_wire::conventions::tenant_id(&app_id, &env.name);
    let caller = match peer {
        Some(ConnectInfo(addr)) => format!("{} from {addr}", principal.label()),
        None => format!("{} from an unknown address", principal.label()),
    };

    let Some(store_endpoint) = state.storage.store.clone() else {
        warn!(
            app_id = %app_id,
            environment = %env.name,
            %caller,
            "Refused a storage token because this control plane has no store_endpoint; there is nowhere for this tenant's kv, database or blob data to live, so no token would be usable"
        );
        return Err(ApiError::NotConfigured(format!(
            "This control plane has no deis-store configured, so {tenant} has nowhere to keep kv, \
             database or blob data. Set store_endpoint in the deis-control configuration file to \
             the address deis-store listens on, and restart it."
        )));
    };

    let Some(token) = state.storage.token_for(&app_id, &env.name) else {
        warn!(
            app_id = %app_id,
            environment = %env.name,
            %caller,
            store_endpoint = %store_endpoint,
            "Refused a storage token because storage_auth_secret is unset; this control plane mints no capability tokens, so its own deployments carry none either and deis-store will refuse them if it has an auth_secret"
        );
        return Err(ApiError::NotConfigured(format!(
            "This control plane has no storage_auth_secret, so it mints no capability token for \
             {tenant} and cannot hand one out. Set storage_auth_secret in the deis-control \
             configuration file to the identical string as auth_secret on deis-store and \
             deis-queue, and restart all three."
        )));
    };

    // The mint is also a row in the audit log, written by the gate that let
    // this call through (action `storage_token.mint`, recorded whatever the
    // outcome). This line is here as well because it is the one an operator
    // finds while reading the control plane's log for something else, and
    // what was handed out - a credential that talks to deis-store directly
    // and does not expire - is worth meeting twice.
    info!(
        app_id = %app_id,
        environment = %env.name,
        tenant = %tenant,
        %caller,
        store_endpoint = %store_endpoint,
        "Minted a storage capability token; the caller can now read and write this tenant's kv, database and blob data directly against deis-store"
    );

    Ok(Json(StorageTokenResponse {
        tenant,
        store_endpoint,
        token,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        Harness, TEST_STORAGE_SECRET, harness_with_storage, json, storage_wiring,
    };

    use axum::http::StatusCode;

    const ROUTE: &str = "/api/apps/demo/environments/production/storage-token";

    /// An app with one environment, on a control plane wired the way the
    /// `storage` argument says.
    async fn platform(storage: crate::delivery::StorageWiring) -> Harness {
        let h = harness_with_storage(true, vec!["node-psk".to_string()], storage).await;
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

    /// The whole point of the endpoint: what it answers has to be the token
    /// deis-store will recompute for that tenant, or the CLI is handed a
    /// credential that is refused on its first call. Recomputed here from the
    /// convention rather than compared against a literal, because the literal
    /// would still match after the derivation changed under it.
    #[tokio::test]
    async fn the_token_is_the_one_deis_store_will_check_for_that_tenant() {
        let h = platform(storage_wiring(Some(TEST_STORAGE_SECRET))).await;
        let (status, body) = json(&h.router, "GET", ROUTE, serde_json::Value::Null).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["tenant"], "demo.production");
        assert_eq!(body["store_endpoint"], "http://127.0.0.1:8090");
        assert_eq!(
            body["token"],
            deis_wire::conventions::storage_token(TEST_STORAGE_SECRET, "demo.production")
        );
        assert!(
            deis_wire::conventions::storage_token_matches(
                &deis_wire::conventions::storage_token(TEST_STORAGE_SECRET, "demo.production"),
                body["token"].as_str().unwrap(),
            ),
            "the answer has to pass the check deis-store itself runs"
        );
    }

    /// A token minted for the wrong environment is refused by deis-store on
    /// every call, so the two environments of one app must not share one.
    #[tokio::test]
    async fn each_environment_gets_its_own_token() {
        let h = platform(storage_wiring(Some(TEST_STORAGE_SECRET))).await;
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
        assert_eq!(preview["tenant"], "demo.preview");
        assert_ne!(production["token"], preview["token"]);
    }

    /// The refusal has to name the key, because the person reading it is
    /// looking at a CLI error and cannot see the control plane's log.
    #[tokio::test]
    async fn a_platform_with_no_storage_secret_says_which_key_to_set() {
        let h = platform(storage_wiring(None)).await;
        let (status, body) = json(&h.router, "GET", ROUTE, serde_json::Value::Null).await;

        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let message = body["error"].as_str().unwrap();
        assert!(message.contains("storage_auth_secret"), "{message}");
        assert!(message.contains("demo.production"), "{message}");
    }

    #[tokio::test]
    async fn a_platform_with_no_store_says_which_key_to_set() {
        let h = platform(crate::delivery::StorageWiring::default()).await;
        let (status, body) = json(&h.router, "GET", ROUTE, serde_json::Value::Null).await;

        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("store_endpoint"),
            "{body}"
        );
    }

    /// An environment that does not exist must not be mintable: the token is
    /// derived from the name, so answering would hand out a working
    /// credential for a tenant nobody deployed.
    #[tokio::test]
    async fn an_unknown_environment_is_not_minted_for() {
        let h = platform(storage_wiring(Some(TEST_STORAGE_SECRET))).await;
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
