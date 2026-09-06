//! What the two ways of cancelling a provider workflow share.
//!
//! A provider `deploy` or `rollback` operation can be stopped through its
//! scoped route, `POST /apps/{app}/environments/{env}/provider-deployments/{id}/cancel`
//! (`api::catalog::providers::cancel_provider_deployment`), or through the
//! generic `POST /operations/{id}/cancel` (`api::migrations::cancel_operation`),
//! which dispatches on the operation's kind. Both have to mean exactly the
//! same thing - the same `If-Match` fence, the same optional `Idempotency-Key`
//! replay, the same refusal codes, the same answer - so the one place that
//! says what a cancel is lives here and both handlers call it.

use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use deis_wire::{CanonicalId, ProductOperation};
use tracing::info;

use crate::api::catalog::actor_subject;
use crate::api::catalog::providers::{keyed_operation_context, operation_headers};
use crate::api::error::ApiResult;
use crate::api::headers::{IDEMPOTENCY_KEY, optional_header};
use crate::api::paths::ApiPaths;
use crate::catalog::{CatalogStore, OperationScopeNames, ProviderDeploymentCancellation};
use crate::identity::Principal;

/// One cancel request for a provider workflow, as either route received it.
pub(crate) struct ProviderWorkflowCancel<'a> {
    /// The application and environment the workflow belongs to. The scoped
    /// route takes them from its path; the generic route resolves them from
    /// the operation record, after checking the caller against them.
    pub scope: &'a OperationScopeNames,
    pub operation_id: CanonicalId,
    /// The operation revision the caller saw (`If-Match`), already parsed by
    /// the route, so each route keeps its own wording for a missing or
    /// malformed header.
    pub expected_revision: u64,
    /// The route the request came in on, in the legacy spelling the
    /// idempotency fingerprint is built from. Two routes are two requests:
    /// a key replayed across them is refused as a different request.
    pub fingerprint_route: &'a str,
}

/// Ask a provider workflow to stop and answer the way the scoped route does.
///
/// `Location` names the scoped provider deployment rather than the generic
/// operation, whichever route the request came in on: the credential that
/// may cancel a deployment is the one that may read it there, while the
/// generic operation read is an administrator's.
pub(crate) async fn cancel_provider_workflow(
    store: &CatalogStore,
    principal: &Principal,
    headers: &HeaderMap,
    paths: ApiPaths,
    cancel: ProviderWorkflowCancel<'_>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProductOperation>)> {
    let replay = optional_header(headers, IDEMPOTENCY_KEY)?
        .map(|key| keyed_operation_context(headers, principal, cancel.fingerprint_route, key))
        .transpose()?;
    let operation = store
        .request_provider_deployment_cancellation(ProviderDeploymentCancellation {
            application_id: cancel.scope.application_id,
            environment_id: cancel.scope.environment_id,
            operation_id: cancel.operation_id,
            expected_revision: cancel.expected_revision,
            actor_subject: actor_subject(principal),
            replay,
        })
        .await?;
    info!(
        operation_id = %cancel.operation_id,
        kind = operation.kind.as_str(),
        state = operation.state.as_str(),
        app = %cancel.scope.application_name,
        environment = %cancel.scope.environment_name,
        route = cancel.fingerprint_route,
        caller = %principal.label(),
        "Provider workflow cancel answered"
    );
    let headers = operation_headers(
        paths,
        &operation,
        &cancel.scope.application_name,
        &cancel.scope.environment_name,
    )?;
    Ok((StatusCode::ACCEPTED, headers, Json(operation)))
}
