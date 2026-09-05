use std::fmt;

use async_trait::async_trait;

use crate::{
    DriverError, DriverNextAction, DriverOperation, DriverReceipt, DriverRequest, DriverStepState,
    SideEffect, ValidatedProviderDriverManifest,
};

#[async_trait]
pub trait ProviderDriver: Send + Sync {
    fn manifest(&self) -> &ValidatedProviderDriverManifest;

    /// Execute exactly one durable lifecycle operation. Credentials are an
    /// implementation concern and must be obtained inside the driver security
    /// boundary, never added to [`DriverRequest`].
    async fn execute(&self, request: &DriverRequest) -> Result<DriverReceipt, DriverError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverContractError(&'static str);

impl DriverContractError {
    pub(crate) const fn new(message: &'static str) -> Self {
        Self(message)
    }
}

impl fmt::Display for DriverContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for DriverContractError {}

/// Validate a persisted request before handing it to provider code.
pub fn validate_driver_request(
    manifest: &ValidatedProviderDriverManifest,
    request: &DriverRequest,
) -> Result<(), DriverContractError> {
    if request.schema_version != 1 {
        return Err(DriverContractError(
            "driver request must use schemaVersion 1",
        ));
    }
    if request.target.backend_id != manifest.get().backend.id {
        return Err(DriverContractError(
            "driver request target backend does not match the selected driver",
        ));
    }
    if matches!(
        request.operation,
        DriverOperation::Plan
            | DriverOperation::Prepare
            | DriverOperation::Upload
            | DriverOperation::Verify
            | DriverOperation::Activate
    ) && request.artifact.is_none()
    {
        return Err(DriverContractError(
            "artifact-dependent driver operation needs exact artifact lineage",
        ));
    }
    Ok(())
}

/// Validate the identity and recovery guarantees at the boundary before a
/// receipt is persisted or returned to a caller.
pub fn validate_driver_response(
    request: &DriverRequest,
    receipt: &DriverReceipt,
) -> Result<(), DriverContractError> {
    if request.schema_version != 1 || receipt.schema_version != 1 {
        return Err(DriverContractError(
            "driver request and receipt must use schemaVersion 1",
        ));
    }
    if receipt.operation_id != request.operation_id
        || receipt.operation != request.operation
        || receipt.desired_state_digest != request.desired_state_digest
    {
        return Err(DriverContractError(
            "driver receipt does not bind the exact operation and desired state",
        ));
    }
    if receipt.state == DriverStepState::Unknown
        && (receipt.side_effect == SideEffect::None
            || !matches!(
                receipt.next_action,
                DriverNextAction::Observe | DriverNextAction::Reconcile
            ))
    {
        return Err(DriverContractError(
            "unknown provider result must preserve possible side effects and query before retry",
        ));
    }
    if receipt.state == DriverStepState::Failed
        && receipt.side_effect == SideEffect::Possible
        && !matches!(
            receipt.next_action,
            DriverNextAction::Observe
                | DriverNextAction::Reconcile
                | DriverNextAction::Rollback
                | DriverNextAction::ManualIntervention
        )
    {
        return Err(DriverContractError(
            "failed side-effecting provider result may not be retried without observation",
        ));
    }
    if receipt.state == DriverStepState::Succeeded && receipt.next_action == DriverNextAction::Retry
    {
        return Err(DriverContractError(
            "successful driver result cannot request retry",
        ));
    }
    if receipt.state == DriverStepState::Succeeded
        && receipt.operation == DriverOperation::Upload
        && (receipt
            .provider_version_id
            .as_deref()
            .is_none_or(str::is_empty)
            || receipt.side_effect != SideEffect::Confirmed)
    {
        return Err(DriverContractError(
            "successful upload must identify the immutable provider version and confirmed side effect",
        ));
    }
    if receipt.state == DriverStepState::Succeeded
        && receipt.operation == DriverOperation::Activate
        && (receipt
            .provider_deployment_id
            .as_deref()
            .is_none_or(str::is_empty)
            || receipt.side_effect != SideEffect::Confirmed)
    {
        return Err(DriverContractError(
            "successful activation must identify the provider deployment and confirmed side effect",
        ));
    }
    if [
        receipt.provider_request_id.as_deref(),
        receipt.provider_version_id.as_deref(),
        receipt.provider_deployment_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .chain(receipt.provider_resource_ids.values().map(String::as_str))
    .any(|value| value.len() > 2048 || value.chars().any(char::is_control))
    {
        return Err(DriverContractError(
            "provider identifiers must be bounded and contain no control characters",
        ));
    }
    if receipt.diagnostic.as_ref().is_some_and(|diagnostic| {
        diagnostic.len() > 16 * 1024
            || diagnostic
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    }) {
        return Err(DriverContractError(
            "provider diagnostic exceeds its bound or contains unsafe control characters",
        ));
    }
    Ok(())
}

/// Apply the same exact-identity and query-before-retry rules to a driver's
/// error path. A process exit cannot erase the durable operation it belonged
/// to or turn an ambiguous provider mutation into a blind retry.
pub fn validate_driver_error(
    request: &DriverRequest,
    error: &DriverError,
) -> Result<(), DriverContractError> {
    if error.operation_id != request.operation_id
        || error.operation != request.operation
        || error.desired_state_digest != request.desired_state_digest
    {
        return Err(DriverContractError(
            "driver error does not bind the exact operation and desired state",
        ));
    }
    if error.side_effect == SideEffect::Possible
        && !matches!(
            error.next_action,
            DriverNextAction::Observe
                | DriverNextAction::Reconcile
                | DriverNextAction::Rollback
                | DriverNextAction::ManualIntervention
        )
    {
        return Err(DriverContractError(
            "ambiguous provider error may not be retried before observation",
        ));
    }
    if error.retryable && error.next_action == DriverNextAction::None {
        return Err(DriverContractError(
            "retryable provider error must identify its next action",
        ));
    }
    if !error.retryable && error.next_action == DriverNextAction::Retry {
        return Err(DriverContractError(
            "non-retryable provider error cannot request retry",
        ));
    }
    if error.code.is_empty()
        || error.code.len() > 128
        || !error
            .code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        || error.message.is_empty()
        || error.message.len() > 16 * 1024
        || error
            .message
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        || error.provider_request_id.as_ref().is_some_and(|value| {
            value.is_empty() || value.len() > 2048 || value.chars().any(char::is_control)
        })
    {
        return Err(DriverContractError(
            "provider error code or diagnostic is not bounded canonical text",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use deis_wire::CanonicalId;

    use super::*;
    use crate::{DriverOperation, ProviderNeutralPlan, ProviderTargetRef, Sha256Digest};

    fn request() -> DriverRequest {
        DriverRequest {
            schema_version: 1,
            operation_id: CanonicalId::new_v7(),
            operation: DriverOperation::Upload,
            desired_state_digest: "a".repeat(64).parse::<Sha256Digest>().unwrap(),
            application_id: CanonicalId::new_v7(),
            environment_id: CanonicalId::new_v7(),
            target: ProviderTargetRef {
                provider_target_id: CanonicalId::new_v7(),
                provider_installation_id: CanonicalId::new_v7(),
                backend_id: "cloudflare-workers-pages".to_owned(),
                target_config_digest: "b".repeat(64).parse().unwrap(),
            },
            artifact: None,
            plan: ProviderNeutralPlan::default(),
        }
    }

    fn receipt(request: &DriverRequest) -> DriverReceipt {
        DriverReceipt {
            schema_version: 1,
            operation_id: request.operation_id,
            operation: request.operation,
            desired_state_digest: request.desired_state_digest.clone(),
            state: DriverStepState::Succeeded,
            deployment_state: None,
            side_effect: SideEffect::Confirmed,
            provider_request_id: None,
            provider_version_id: Some("provider-version".to_owned()),
            provider_deployment_id: None,
            provider_resource_ids: BTreeMap::new(),
            observed_at: Utc::now(),
            next_action: DriverNextAction::None,
            diagnostic: None,
        }
    }

    #[test]
    fn receipt_is_bound_to_exact_request() {
        let request = request();
        let mut receipt = receipt(&request);
        assert_eq!(validate_driver_response(&request, &receipt), Ok(()));
        receipt.operation_id = CanonicalId::new_v7();
        assert!(validate_driver_response(&request, &receipt).is_err());
    }

    #[test]
    fn ambiguous_side_effect_must_be_observed_before_retry() {
        let request = request();
        let mut receipt = receipt(&request);
        receipt.state = DriverStepState::Unknown;
        receipt.side_effect = SideEffect::Possible;
        receipt.next_action = DriverNextAction::Retry;
        assert!(validate_driver_response(&request, &receipt).is_err());
        receipt.next_action = DriverNextAction::Reconcile;
        assert_eq!(validate_driver_response(&request, &receipt), Ok(()));
    }

    #[test]
    fn artifact_operations_and_backend_selection_are_exact() {
        let manifest = ValidatedProviderDriverManifest::parse_json(include_bytes!(
            "../../../contracts/v1/fixtures/valid/provider-driver.json"
        ))
        .unwrap();
        let mut request = request();
        assert!(validate_driver_request(&manifest, &request).is_err());
        request.operation = DriverOperation::Observe;
        assert_eq!(validate_driver_request(&manifest, &request), Ok(()));
        request.target.backend_id = "deislet".to_owned();
        assert!(validate_driver_request(&manifest, &request).is_err());
    }

    #[test]
    fn successful_upload_cannot_lose_the_provider_version_identity() {
        let request = request();
        let mut receipt = receipt(&request);
        receipt.provider_version_id = None;
        assert!(validate_driver_response(&request, &receipt).is_err());
        receipt.provider_version_id = Some("version-1".to_owned());
        assert_eq!(validate_driver_response(&request, &receipt), Ok(()));
    }

    #[test]
    fn error_path_keeps_identity_and_forbids_blind_ambiguous_retry() {
        let request = request();
        let mut error = DriverError {
            operation_id: request.operation_id,
            operation: request.operation,
            desired_state_digest: request.desired_state_digest.clone(),
            category: crate::DriverErrorCategory::UnknownState,
            code: "PROVIDER_RESULT_UNKNOWN".to_owned(),
            message: "upload response was lost".to_owned(),
            retryable: true,
            side_effect: SideEffect::Possible,
            next_action: DriverNextAction::Retry,
            provider_request_id: None,
        };
        assert!(validate_driver_error(&request, &error).is_err());
        error.next_action = DriverNextAction::Reconcile;
        assert_eq!(validate_driver_error(&request, &error), Ok(()));
        error.operation_id = CanonicalId::new_v7();
        assert!(validate_driver_error(&request, &error).is_err());
    }
}
