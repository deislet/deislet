//! The capability `deis logs`, `deis tail` and `deis trace` read with.
//!
//! deis-telemetry accepts only capabilities this control plane signed, and a
//! reader's capability names which tenants it may read ([`TELEMETRY_READ`]):
//! a query or a follow stream hands back those tenants' records and nothing
//! else. So the read credential is minted here, per call, for exactly what
//! the caller's own scopes reach, the same way `deis kv` gets its storage
//! token - there is no shared secret left for a CLI to derive one from.
//!
//! Three routes, one per breadth, each gated on the scope that breadth
//! needs:
//!
//! * one environment: `read:<app>/<env>`;
//! * every environment of one application: `read:<app>/*`, because that is
//!   what `deis logs --all-environments` shows;
//! * the platform: `read:*/*`, minting [`TELEMETRY_PLATFORM_TENANT`], which
//!   reads every tenant and the nodes' own records and is the one capability
//!   deis-telemetry's Prometheus page accepts.
//!
//! Plain `read` is the verb because reading telemetry is looking, and looking
//! is what `read` grants; what this adds is a credential that looks without
//! passing through this process again, so every mint is recorded in the
//! audit log whatever its outcome (action `telemetry_token.mint` or
//! `telemetry_token.mint_platform`), exactly like the storage-token mint.

use std::net::SocketAddr;

use axum::{
    Extension, Json,
    extract::{ConnectInfo, Path, State},
};
use deis_capability::{
    AUD_TELEMETRY, CapabilityResource, TELEMETRY_PLATFORM_TENANT, TELEMETRY_READ,
};
use serde::Serialize;
use tracing::info;
use utoipa::ToSchema;

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};
use crate::api::openapi;
use crate::identity::Principal;

/// A read credential for deis-telemetry.
#[derive(Debug, Serialize, ToSchema)]
pub struct TelemetryTokenResponse {
    /// The canonical tenants (`v1/<application id>/<environment id>`) the
    /// token reads, or `@platform` for a token that reads everything.
    pub tenants: Vec<String>,
    /// The capability, for the `Authorization: Bearer` header on calls to
    /// deis-telemetry. Valid for 15 minutes; it may query and follow, and
    /// may not push.
    pub token: String,
}

/// Mint a telemetry read credential for one environment.
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/environments/{env_name}/telemetry-token",
    operation_id = "getEnvironmentTelemetryToken",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("env_name" = String, Path, description = "The environment's name.")),
    responses(
        (status = 200, description = "A credential that reads this environment's telemetry.", body = TelemetryTokenResponse),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn environment_telemetry_token(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
    peer: Option<ConnectInfo<SocketAddr>>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<TelemetryTokenResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let tenants =
        vec![super::assignments::canonical_tenant_of(&state, &env.app_id, &env.id).await?];
    Ok(Json(mint(&state, &principal, peer, tenants).await?))
}

/// Mint a telemetry read credential for every environment of an application.
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/telemetry-token",
    operation_id = "getAppTelemetryToken",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id.")),
    responses(
        (status = 200, description = "A credential that reads the telemetry of every environment of the application.", body = TelemetryTokenResponse),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn app_telemetry_token(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
    peer: Option<ConnectInfo<SocketAddr>>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<TelemetryTokenResponse>> {
    let app = state.store.get_app(&app_id).await?;
    let tenants = state
        .store
        .list_environments(&app_id)
        .await?
        .iter()
        .map(|env| {
            deis_wire::CanonicalTenantId::new(app.canonical_id, env.canonical_id).to_string()
        })
        .collect();
    Ok(Json(mint(&state, &principal, peer, tenants).await?))
}

/// Mint a telemetry read credential for the whole platform.
#[utoipa::path(
    get,
    path = "/api/platform/telemetry-token",
    operation_id = "getPlatformTelemetryToken",
    tag = "platform",
    responses(
        (status = 200, description = "A credential that reads every tenant's telemetry and the nodes' own.", body = TelemetryTokenResponse),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn platform_telemetry_token(
    State(state): State<CatalogState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<TelemetryTokenResponse>> {
    let tenants = vec![TELEMETRY_PLATFORM_TENANT.to_string()];
    Ok(Json(mint(&state, &principal, peer, tenants).await?))
}

/// Sign a read capability for `tenants`. An application with no environment
/// yet gets a token that reads nothing, which is what it has.
async fn mint(
    state: &CatalogState,
    principal: &Principal,
    peer: Option<ConnectInfo<SocketAddr>>,
    tenants: Vec<String>,
) -> ApiResult<TelemetryTokenResponse> {
    let subject = format!(
        "spiffe://deislet/human/{}",
        principal.account.as_deref().unwrap_or("unknown")
    );
    let resources = tenants
        .iter()
        .map(|tenant| CapabilityResource {
            tenant: tenant.clone(),
            binding: TELEMETRY_READ.to_string(),
            generation: None,
        })
        .collect();
    let token = state
        .issuer
        .issue(AUD_TELEMETRY, &subject, resources)
        .await
        .map_err(|error| {
            ApiError::InternalError(format!(
                "the telemetry capability could not be issued: {error}"
            ))
        })?;
    let caller = match peer {
        Some(ConnectInfo(addr)) => format!("{} from {addr}", principal.label()),
        None => format!("{} from an unknown address", principal.label()),
    };
    // The audit row is written by the gate that let this call through; this
    // line is for whoever reads the control plane's own log.
    info!(
        tenants = ?tenants,
        %caller,
        subject = %subject,
        "Minted a telemetry read capability; for its 15 minutes the caller can query and follow \
         these tenants' logs, traces and metrics directly against deis-telemetry"
    );
    Ok(TelemetryTokenResponse { tenants, token })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{harness_with_storage, json, storage_wiring};

    use axum::http::StatusCode;
    use deis_capability::{
        AUD_TELEMETRY, CapabilityVerifier, TELEMETRY_INGEST, TELEMETRY_PLATFORM_TENANT,
        TELEMETRY_READ,
    };

    /// The canonical tenant of one of `demo`'s environments.
    async fn tenant(h: &super::super::tests::Harness, env: &str) -> String {
        let environment = super::super::apps::resolve_environment(&h.state, "demo", env)
            .await
            .unwrap();
        super::super::assignments::canonical_tenant_of(&h.state, "demo", &environment.id)
            .await
            .unwrap()
    }

    async fn platform() -> super::super::tests::Harness {
        let h = harness_with_storage(true, storage_wiring()).await;
        json(
            &h.router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        for env in ["production", "preview"] {
            json(
                &h.router,
                "POST",
                "/api/apps/demo/environments",
                serde_json::json!({"name": env}),
            )
            .await;
        }
        h
    }

    #[tokio::test]
    async fn an_environment_token_reads_that_environment_only() {
        let h = platform().await;
        let (status, body) = json(
            &h.router,
            "GET",
            "/api/apps/demo/environments/production/telemetry-token",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let production = tenant(&h, "production").await;
        assert_eq!(body["tenants"], serde_json::json!([production]));

        let mut verifier = CapabilityVerifier::new(h.state.issuer.verifying_key().await);
        verifier
            .apply_revocation_list(deis_capability::RevocationList::new("iss", 0, vec![]))
            .unwrap();
        let token = body["token"].as_str().unwrap();
        let grant = verifier
            .verify_grant_with_grace(token, AUD_TELEMETRY)
            .expect("deis-telemetry accepts it");
        assert_eq!(
            grant.tenants_with(TELEMETRY_READ).collect::<Vec<_>>(),
            [production.as_str()]
        );
        assert_eq!(
            grant.tenants_with(TELEMETRY_INGEST).count(),
            0,
            "a reader's token must not write"
        );
    }

    #[tokio::test]
    async fn an_application_token_reads_every_environment() {
        let h = platform().await;
        let (status, body) = json(
            &h.router,
            "GET",
            "/api/apps/demo/telemetry-token",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let mut tenants: Vec<String> = serde_json::from_value(body["tenants"].clone()).unwrap();
        tenants.sort();
        let mut expected = vec![tenant(&h, "preview").await, tenant(&h, "production").await];
        expected.sort();
        assert_eq!(tenants, expected);

        let (status, _) = json(
            &h.router,
            "GET",
            "/api/apps/nothing-here/telemetry-token",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_platform_token_reads_everything() {
        let h = platform().await;
        let (status, body) = json(
            &h.router,
            "GET",
            "/api/telemetry-token",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["tenants"],
            serde_json::json!([TELEMETRY_PLATFORM_TENANT])
        );
    }
}
