//! Revocation-list endpoint for T8 degraded-mode polling.
//!
//! Each data-plane service (store, queue, telemetry) polls this endpoint
//! every 30 seconds to discover the current `auth_epoch` and any individually
//! revoked JTIs.  A successful poll resets the service's degraded-mode clock.

use axum::{Json, extract::State};
use serde::Serialize;

use crate::capability_issuer::CapabilityIssuer;

/// The current revocation state the control plane wants every verifier to know.
#[derive(Debug, Serialize)]
pub struct RevocationList {
    /// Minimum `auth_epoch` a capability must carry to be accepted.
    pub auth_epoch: u64,
    /// JTIs that have been individually revoked.  Empty until individual
    /// revocation is implemented (auth_epoch bump covers mass revocation).
    pub revoked_jtis: Vec<String>,
}

/// State held by the revocation-list handler.
#[derive(Clone)]
pub struct RevocationState {
    pub issuer: CapabilityIssuer,
}

/// `GET /api/v1/capability/revocation-list`
///
/// Returns the current `auth_epoch` and an empty revoked JTI list.  This is
/// the minimal viable control-plane endpoint: individual JTI revocation is
/// not yet implemented, and mass revocation is covered by bumping the
/// `auth_epoch` through the existing `CapabilityIssuer::bump_auth_epoch`.
pub async fn revocation_list(
    State(state): State<RevocationState>,
) -> Json<RevocationList> {
    let auth_epoch = state.issuer.auth_epoch().await;
    Json(RevocationList {
        auth_epoch,
        revoked_jtis: Vec::new(),
    })
}