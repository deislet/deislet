use std::{fmt, time::Duration};

use async_trait::async_trait;
use deis_wire::CanonicalId;
use tokio::time::{Instant, interval_at};

use crate::{
    DriverContractError, DriverError, DriverExecutionContext, DriverReceipt, DriverRequest,
    ProviderDriver, Sha256Digest, validate_driver_error, validate_driver_request,
    validate_driver_response,
};

/// Exact executable/manifest identity selected with a durable target. A
/// request may only be handed to the driver whose pin is recorded here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderDriverPin {
    pub driver_id: String,
    pub driver_version: String,
    pub distribution_digest: Sha256Digest,
    pub manifest_digest: Sha256Digest,
}

/// A fenced claim returned by the durable operation store. `claim_epoch`
/// advances every time an expired operation is taken over, so an old worker
/// cannot settle after a replacement has begun reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverOperationClaim {
    pub operation_id: CanonicalId,
    pub owner_id: CanonicalId,
    pub claim_epoch: u64,
    pub driver_pin: ProviderDriverPin,
    /// The immutable operation originally requested by the product workflow.
    pub original_operation: crate::DriverOperation,
    /// The current execution request. After an expired/unknown attempt the
    /// store changes only this operation to `reconcile`; identity and desired
    /// state remain exact.
    pub request: DriverRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverOperationClaimOutcome {
    Acquired(Box<DriverOperationClaim>),
    Busy,
    NotRunnable,
}

impl DriverOperationClaimOutcome {
    #[must_use]
    pub fn into_claim(self) -> Option<DriverOperationClaim> {
        match self {
            Self::Acquired(claim) => Some(*claim),
            Self::Busy | Self::NotRunnable => None,
        }
    }
}

#[async_trait]
pub trait DriverOperationStore: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn claim(
        &self,
        operation_id: CanonicalId,
        owner_id: CanonicalId,
        lease_duration: Duration,
    ) -> Result<DriverOperationClaimOutcome, Self::Error>;

    async fn renew(
        &self,
        claim: &DriverOperationClaim,
        lease_duration: Duration,
    ) -> Result<(), Self::Error>;

    async fn record_receipt(
        &self,
        claim: &DriverOperationClaim,
        receipt: &DriverReceipt,
    ) -> Result<(), Self::Error>;

    async fn record_driver_error(
        &self,
        claim: &DriverOperationClaim,
        error: &DriverError,
    ) -> Result<(), Self::Error>;

    async fn record_contract_failure(
        &self,
        claim: &DriverOperationClaim,
        message: &str,
    ) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverOrchestratorConfig {
    pub lease_duration: Duration,
    pub heartbeat_interval: Duration,
}

impl Default for DriverOrchestratorConfig {
    fn default() -> Self {
        Self {
            lease_duration: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(10),
        }
    }
}

impl DriverOrchestratorConfig {
    pub fn validate(self) -> Result<Self, DriverOrchestratorConfigError> {
        if self.heartbeat_interval.is_zero()
            || self.lease_duration.is_zero()
            || self
                .heartbeat_interval
                .checked_mul(2)
                .is_none_or(|minimum| minimum > self.lease_duration)
        {
            return Err(DriverOrchestratorConfigError);
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverOrchestratorConfigError;

impl fmt::Display for DriverOrchestratorConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "provider driver heartbeat must be non-zero and no more than half the lease duration",
        )
    }
}

impl std::error::Error for DriverOrchestratorConfigError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverOrchestrationOutcome {
    Succeeded(DriverReceipt),
    DriverFailed(DriverError),
}

#[derive(Debug)]
pub enum DriverOrchestratorError<E> {
    Store(E),
    AlreadyClaimed { operation_id: CanonicalId },
    NotRunnable { operation_id: CanonicalId },
    Contract(DriverContractError),
}

impl<E: fmt::Display> fmt::Display for DriverOrchestratorError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "provider operation store failed: {error}"),
            Self::AlreadyClaimed { operation_id } => {
                write!(
                    formatter,
                    "provider operation {operation_id} has a live owner"
                )
            }
            Self::NotRunnable { operation_id } => {
                write!(
                    formatter,
                    "provider operation {operation_id} is not runnable"
                )
            }
            Self::Contract(error) => {
                write!(formatter, "provider driver violated its contract: {error}")
            }
        }
    }
}

impl<E> std::error::Error for DriverOrchestratorError<E> where E: std::error::Error + 'static {}

pub struct DriverOrchestrator<S> {
    store: S,
    config: DriverOrchestratorConfig,
}

impl<S> DriverOrchestrator<S>
where
    S: DriverOperationStore,
{
    pub fn new(
        store: S,
        config: DriverOrchestratorConfig,
    ) -> Result<Self, DriverOrchestratorConfigError> {
        Ok(Self {
            store,
            config: config.validate()?,
        })
    }

    /// Claim and execute one exact durable driver request. All terminal paths
    /// are recorded while the same owner/epoch fence is live. If a heartbeat
    /// write fails, the driver future is dropped and no stale terminal result
    /// is written; an expired replacement must inspect provider state first.
    pub async fn execute(
        &self,
        operation_id: CanonicalId,
        owner_id: CanonicalId,
        driver: &dyn ProviderDriver,
    ) -> Result<DriverOrchestrationOutcome, DriverOrchestratorError<S::Error>> {
        let claim = match self
            .store
            .claim(operation_id, owner_id, self.config.lease_duration)
            .await
            .map_err(DriverOrchestratorError::Store)?
        {
            DriverOperationClaimOutcome::Acquired(claim) => *claim,
            DriverOperationClaimOutcome::Busy => {
                return Err(DriverOrchestratorError::AlreadyClaimed { operation_id });
            }
            DriverOperationClaimOutcome::NotRunnable => {
                return Err(DriverOrchestratorError::NotRunnable { operation_id });
            }
        };
        if claim.operation_id != operation_id
            || claim.owner_id != owner_id
            || claim.claim_epoch == 0
            || claim.request.operation_id != operation_id
            || (claim.request.operation != claim.original_operation
                && claim.request.operation != crate::DriverOperation::Reconcile)
        {
            let error = DriverContractError::new(
                "durable store returned a mismatched or non-reconciliation claim",
            );
            self.store
                .record_contract_failure(&claim, &error.to_string())
                .await
                .map_err(DriverOrchestratorError::Store)?;
            return Err(DriverOrchestratorError::Contract(error));
        }
        let actual_manifest_digest = match driver.manifest().canonical_sha256() {
            Ok(digest) => digest,
            Err(_) => {
                let error = DriverContractError::new(
                    "selected provider driver manifest cannot be canonicalized",
                );
                self.store
                    .record_contract_failure(&claim, &error.to_string())
                    .await
                    .map_err(DriverOrchestratorError::Store)?;
                return Err(DriverOrchestratorError::Contract(error));
            }
        };
        let actual_driver = &driver.manifest().get().driver;
        if claim.driver_pin.driver_id != actual_driver.id
            || claim.driver_pin.driver_version != actual_driver.version
            || claim.driver_pin.distribution_digest != actual_driver.distribution_digest.value
            || claim.driver_pin.manifest_digest != actual_manifest_digest
        {
            let error = DriverContractError::new(
                "selected provider driver does not match the durable target pin",
            );
            self.store
                .record_contract_failure(&claim, &error.to_string())
                .await
                .map_err(DriverOrchestratorError::Store)?;
            return Err(DriverOrchestratorError::Contract(error));
        }
        if let Err(error) = validate_driver_request(driver.manifest(), &claim.request) {
            self.store
                .record_contract_failure(&claim, &error.to_string())
                .await
                .map_err(DriverOrchestratorError::Store)?;
            return Err(DriverOrchestratorError::Contract(error));
        }
        if let Err(error) = driver.verify_before_execution().await {
            self.store
                .record_contract_failure(&claim, &error.to_string())
                .await
                .map_err(DriverOrchestratorError::Store)?;
            return Err(DriverOrchestratorError::Contract(error));
        }

        let execution_context =
            DriverExecutionContext::new(&claim.request, claim.owner_id, claim.claim_epoch);
        let mut execution = Box::pin(driver.execute(&execution_context));
        let mut heartbeat = interval_at(
            Instant::now() + self.config.heartbeat_interval,
            self.config.heartbeat_interval,
        );
        let result = loop {
            tokio::select! {
                result = &mut execution => break result,
                _ = heartbeat.tick() => {
                    self.store
                        .renew(&claim, self.config.lease_duration)
                        .await
                        .map_err(DriverOrchestratorError::Store)?;
                }
            }
        };

        match result {
            Ok(receipt) => {
                if let Err(error) = validate_driver_response(&claim.request, &receipt) {
                    self.store
                        .record_contract_failure(&claim, &error.to_string())
                        .await
                        .map_err(DriverOrchestratorError::Store)?;
                    return Err(DriverOrchestratorError::Contract(error));
                }
                self.store
                    .record_receipt(&claim, &receipt)
                    .await
                    .map_err(DriverOrchestratorError::Store)?;
                Ok(DriverOrchestrationOutcome::Succeeded(receipt))
            }
            Err(error) => {
                if let Err(contract_error) = validate_driver_error(&claim.request, &error) {
                    self.store
                        .record_contract_failure(&claim, &contract_error.to_string())
                        .await
                        .map_err(DriverOrchestratorError::Store)?;
                    return Err(DriverOrchestratorError::Contract(contract_error));
                }
                self.store
                    .record_driver_error(&claim, &error)
                    .await
                    .map_err(DriverOrchestratorError::Store)?;
                Ok(DriverOrchestrationOutcome::DriverFailed(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use async_trait::async_trait;
    use chrono::Utc;
    use tokio::sync::Mutex;

    use super::*;
    use crate::{
        DeploymentState, DriverErrorCategory, DriverNextAction, DriverOperation, DriverStepState,
        ProviderDriverManifest, ProviderNeutralPlan, ProviderTargetRef, Sha256Digest, SideEffect,
        ValidatedProviderDriverManifest,
    };

    #[derive(Debug)]
    struct FakeStoreError(&'static str);

    impl fmt::Display for FakeStoreError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl std::error::Error for FakeStoreError {}

    #[derive(Default)]
    struct FakeState {
        request: Option<DriverRequest>,
        not_runnable: bool,
        owner: Option<CanonicalId>,
        epoch: u64,
        renewals: usize,
        receipt: Option<DriverReceipt>,
        error: Option<DriverError>,
        contract_failure: Option<String>,
    }

    #[derive(Clone, Default)]
    struct FakeStore(Arc<Mutex<FakeState>>);

    #[async_trait]
    impl DriverOperationStore for FakeStore {
        type Error = FakeStoreError;

        async fn claim(
            &self,
            operation_id: CanonicalId,
            owner_id: CanonicalId,
            _lease_duration: Duration,
        ) -> Result<DriverOperationClaimOutcome, Self::Error> {
            let mut state = self.0.lock().await;
            if state.not_runnable {
                return Ok(DriverOperationClaimOutcome::NotRunnable);
            }
            if state.owner.is_some() {
                return Ok(DriverOperationClaimOutcome::Busy);
            }
            let request = state
                .request
                .clone()
                .ok_or(FakeStoreError("missing request"))?;
            if request.operation_id != operation_id {
                return Err(FakeStoreError("wrong operation"));
            }
            state.epoch += 1;
            state.owner = Some(owner_id);
            Ok(DriverOperationClaimOutcome::Acquired(Box::new(
                DriverOperationClaim {
                    operation_id,
                    owner_id,
                    claim_epoch: state.epoch,
                    driver_pin: driver_pin(),
                    original_operation: request.operation,
                    request,
                },
            )))
        }

        async fn renew(
            &self,
            claim: &DriverOperationClaim,
            _lease_duration: Duration,
        ) -> Result<(), Self::Error> {
            let mut state = self.0.lock().await;
            if state.owner != Some(claim.owner_id) || state.epoch != claim.claim_epoch {
                return Err(FakeStoreError("stale claim"));
            }
            state.renewals += 1;
            Ok(())
        }

        async fn record_receipt(
            &self,
            claim: &DriverOperationClaim,
            receipt: &DriverReceipt,
        ) -> Result<(), Self::Error> {
            let mut state = self.0.lock().await;
            check_claim(&state, claim)?;
            state.receipt = Some(receipt.clone());
            state.owner = None;
            Ok(())
        }

        async fn record_driver_error(
            &self,
            claim: &DriverOperationClaim,
            error: &DriverError,
        ) -> Result<(), Self::Error> {
            let mut state = self.0.lock().await;
            check_claim(&state, claim)?;
            state.error = Some(error.clone());
            state.owner = None;
            Ok(())
        }

        async fn record_contract_failure(
            &self,
            claim: &DriverOperationClaim,
            message: &str,
        ) -> Result<(), Self::Error> {
            let mut state = self.0.lock().await;
            check_claim(&state, claim)?;
            state.contract_failure = Some(message.to_owned());
            state.owner = None;
            Ok(())
        }
    }

    fn check_claim(state: &FakeState, claim: &DriverOperationClaim) -> Result<(), FakeStoreError> {
        if state.owner == Some(claim.owner_id) && state.epoch == claim.claim_epoch {
            Ok(())
        } else {
            Err(FakeStoreError("stale claim"))
        }
    }

    struct FakeDriver {
        manifest: ValidatedProviderDriverManifest,
        delay: Duration,
        corrupt_receipt: bool,
        fail: bool,
    }

    struct VerificationRejectingDriver {
        manifest: ValidatedProviderDriverManifest,
    }

    #[async_trait]
    impl ProviderDriver for VerificationRejectingDriver {
        fn manifest(&self) -> &ValidatedProviderDriverManifest {
            &self.manifest
        }

        async fn verify_before_execution(&self) -> Result<(), DriverContractError> {
            Err(DriverContractError::new(
                "installed driver material failed its final integrity check",
            ))
        }

        async fn execute(
            &self,
            _context: &DriverExecutionContext<'_>,
        ) -> Result<DriverReceipt, DriverError> {
            panic!("a driver that failed pre-execution verification must never run")
        }
    }

    #[async_trait]
    impl ProviderDriver for FakeDriver {
        fn manifest(&self) -> &ValidatedProviderDriverManifest {
            &self.manifest
        }

        async fn execute(
            &self,
            context: &DriverExecutionContext<'_>,
        ) -> Result<DriverReceipt, DriverError> {
            let request = context.request();
            tokio::time::sleep(self.delay).await;
            if self.fail {
                return Err(DriverError {
                    operation_id: request.operation_id,
                    operation: request.operation,
                    desired_state_digest: request.desired_state_digest.clone(),
                    category: DriverErrorCategory::TransientProvider,
                    code: "PROVIDER_TEMPORARY".to_owned(),
                    message: "provider asked the caller to try later".to_owned(),
                    retryable: true,
                    side_effect: SideEffect::None,
                    next_action: DriverNextAction::Retry,
                    provider_request_id: Some("request-1".to_owned()),
                });
            }
            Ok(DriverReceipt {
                schema_version: 1,
                operation_id: if self.corrupt_receipt {
                    CanonicalId::new_v7()
                } else {
                    request.operation_id
                },
                operation: request.operation,
                desired_state_digest: request.desired_state_digest.clone(),
                state: DriverStepState::Succeeded,
                deployment_state: Some(DeploymentState::Planned),
                side_effect: SideEffect::None,
                provider_request_id: Some("request-1".to_owned()),
                provider_version_id: None,
                provider_deployment_id: None,
                provider_resource_ids: BTreeMap::new(),
                observed_at: Utc::now(),
                next_action: DriverNextAction::None,
                diagnostic: None,
            })
        }
    }

    fn manifest() -> ValidatedProviderDriverManifest {
        let manifest: ProviderDriverManifest = serde_json::from_slice(include_bytes!(
            "../../../contracts/v1/fixtures/valid/provider-driver.json"
        ))
        .unwrap();
        ValidatedProviderDriverManifest::new(manifest).unwrap()
    }

    fn request() -> DriverRequest {
        DriverRequest {
            schema_version: 1,
            operation_id: CanonicalId::new_v7(),
            operation: DriverOperation::Preflight,
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

    fn driver(delay: Duration) -> FakeDriver {
        FakeDriver {
            manifest: manifest(),
            delay,
            corrupt_receipt: false,
            fail: false,
        }
    }

    fn driver_pin() -> ProviderDriverPin {
        let manifest = manifest();
        ProviderDriverPin {
            driver_id: manifest.get().driver.id.clone(),
            driver_version: manifest.get().driver.version.clone(),
            distribution_digest: manifest.get().driver.distribution_digest.value.clone(),
            manifest_digest: manifest.canonical_sha256().unwrap(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn successful_result_is_heartbeat_protected_and_persisted() {
        let request = request();
        let store = FakeStore::default();
        store.0.lock().await.request = Some(request.clone());
        let orchestrator = DriverOrchestrator::new(
            store.clone(),
            DriverOrchestratorConfig {
                lease_duration: Duration::from_millis(60),
                heartbeat_interval: Duration::from_millis(20),
            },
        )
        .unwrap();
        let outcome = orchestrator
            .execute(
                request.operation_id,
                CanonicalId::new_v7(),
                &driver(Duration::from_millis(45)),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, DriverOrchestrationOutcome::Succeeded(_)));
        let state = store.0.lock().await;
        assert!(state.renewals >= 2);
        assert_eq!(
            state.receipt.as_ref().unwrap().operation_id,
            request.operation_id
        );
    }

    #[tokio::test]
    async fn valid_driver_error_is_persisted_as_an_outcome() {
        let request = request();
        let store = FakeStore::default();
        store.0.lock().await.request = Some(request.clone());
        let mut fake = driver(Duration::ZERO);
        fake.fail = true;
        let orchestrator = DriverOrchestrator::new(store.clone(), Default::default()).unwrap();
        let outcome = orchestrator
            .execute(request.operation_id, CanonicalId::new_v7(), &fake)
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            DriverOrchestrationOutcome::DriverFailed(_)
        ));
        assert_eq!(
            store.0.lock().await.error.as_ref().unwrap().code,
            "PROVIDER_TEMPORARY"
        );
    }

    #[tokio::test]
    async fn corrupt_driver_result_is_recorded_and_never_returned_as_success() {
        let request = request();
        let store = FakeStore::default();
        store.0.lock().await.request = Some(request.clone());
        let mut fake = driver(Duration::ZERO);
        fake.corrupt_receipt = true;
        let orchestrator = DriverOrchestrator::new(store.clone(), Default::default()).unwrap();
        let error = orchestrator
            .execute(request.operation_id, CanonicalId::new_v7(), &fake)
            .await
            .unwrap_err();
        assert!(matches!(error, DriverOrchestratorError::Contract(_)));
        assert!(store.0.lock().await.contract_failure.is_some());
    }

    #[tokio::test]
    async fn pre_execution_verification_failure_is_persisted_before_driver_code_runs() {
        let request = request();
        let store = FakeStore::default();
        store.0.lock().await.request = Some(request.clone());
        let driver = VerificationRejectingDriver {
            manifest: manifest(),
        };
        let orchestrator = DriverOrchestrator::new(store.clone(), Default::default()).unwrap();
        let error = orchestrator
            .execute(request.operation_id, CanonicalId::new_v7(), &driver)
            .await
            .unwrap_err();
        assert!(matches!(error, DriverOrchestratorError::Contract(_)));
        let state = store.0.lock().await;
        assert_eq!(
            state.contract_failure.as_deref(),
            Some("installed driver material failed its final integrity check")
        );
        assert!(state.owner.is_none());
    }

    #[tokio::test]
    async fn driver_must_match_the_exact_durable_target_pin() {
        let request = request();
        let store = FakeStore::default();
        store.0.lock().await.request = Some(request.clone());
        let mut actual_manifest: ProviderDriverManifest = serde_json::from_slice(include_bytes!(
            "../../../contracts/v1/fixtures/valid/provider-driver.json"
        ))
        .unwrap();
        actual_manifest.driver.version = "0.2.0".to_owned();
        let fake = FakeDriver {
            manifest: ValidatedProviderDriverManifest::new(actual_manifest).unwrap(),
            delay: Duration::ZERO,
            corrupt_receipt: false,
            fail: false,
        };
        let orchestrator = DriverOrchestrator::new(store.clone(), Default::default()).unwrap();
        let error = orchestrator
            .execute(request.operation_id, CanonicalId::new_v7(), &fake)
            .await
            .unwrap_err();
        assert!(matches!(error, DriverOrchestratorError::Contract(_)));
        assert!(
            store
                .0
                .lock()
                .await
                .contract_failure
                .as_deref()
                .unwrap()
                .contains("durable target pin")
        );
    }

    #[tokio::test]
    async fn live_owner_is_not_bypassed() {
        let request = request();
        let store = FakeStore::default();
        {
            let mut state = store.0.lock().await;
            state.request = Some(request.clone());
            state.owner = Some(CanonicalId::new_v7());
        }
        let orchestrator = DriverOrchestrator::new(store, Default::default()).unwrap();
        let error = orchestrator
            .execute(
                request.operation_id,
                CanonicalId::new_v7(),
                &driver(Duration::ZERO),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            DriverOrchestratorError::AlreadyClaimed { .. }
        ));
    }

    #[tokio::test]
    async fn terminal_operation_is_not_misreported_as_a_live_owner() {
        let request = request();
        let store = FakeStore::default();
        {
            let mut state = store.0.lock().await;
            state.request = Some(request.clone());
            state.not_runnable = true;
        }
        let orchestrator = DriverOrchestrator::new(store, Default::default()).unwrap();
        let error = orchestrator
            .execute(
                request.operation_id,
                CanonicalId::new_v7(),
                &driver(Duration::ZERO),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, DriverOrchestratorError::NotRunnable { .. }));
    }

    #[test]
    fn heartbeat_configuration_keeps_takeover_margin() {
        assert!(
            DriverOrchestratorConfig {
                lease_duration: Duration::from_secs(10),
                heartbeat_interval: Duration::from_secs(6),
            }
            .validate()
            .is_err()
        );
    }
}
