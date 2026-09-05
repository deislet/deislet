use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{
    CommandExecutionError, CommandExecutionErrorKind, CommandInvocation, CredentialBrokerError,
    CredentialBrokerErrorKind, CredentialLeaseDisposition, DriverContractError, DriverError,
    DriverErrorCategory, DriverExecutionContext, DriverNextAction, DriverOperation, DriverReceipt,
    DriverReleaseComponentRole, InstalledDriverRelease, InstalledReleaseExecutable,
    PinnedCommandRunner, ProviderCredentialBroker, ProviderDriver, SecretInjection, SecretString,
    SideEffect, SideEffectPolicy, ValidatedProviderDriverManifest,
};

pub const DRIVER_PROCESS_PROTOCOL_V1: &str = "edge-canon-provider-driver-v1";
pub const DRIVER_PROCESS_CREDENTIAL_ENV: &str = "EDGE_CANON_PROVIDER_CREDENTIAL";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DriverProcessRequest {
    pub message_type: String,
    pub schema_version: u32,
    pub protocol: String,
    pub request: crate::DriverRequest,
}

impl DriverProcessRequest {
    #[must_use]
    pub fn new(request: crate::DriverRequest) -> Self {
        Self {
            message_type: "request".to_owned(),
            schema_version: 1,
            protocol: DRIVER_PROCESS_PROTOCOL_V1.to_owned(),
            request,
        }
    }

    pub fn validate(self) -> Result<crate::DriverRequest, DriverContractError> {
        if self.message_type != "request"
            || self.schema_version != 1
            || self.protocol != DRIVER_PROCESS_PROTOCOL_V1
        {
            return Err(DriverContractError::new(
                "driver process request does not use the exact v1 protocol",
            ));
        }
        Ok(self.request)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "outcome",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum DriverProcessOutcome {
    Succeeded {
        message_type: String,
        schema_version: u32,
        protocol: String,
        receipt: DriverReceipt,
    },
    Failed {
        message_type: String,
        schema_version: u32,
        protocol: String,
        error: DriverError,
    },
}

impl DriverProcessOutcome {
    #[must_use]
    pub fn succeeded(receipt: DriverReceipt) -> Self {
        Self::Succeeded {
            message_type: "outcome".to_owned(),
            schema_version: 1,
            protocol: DRIVER_PROCESS_PROTOCOL_V1.to_owned(),
            receipt,
        }
    }

    #[must_use]
    pub fn failed(error: DriverError) -> Self {
        Self::Failed {
            message_type: "outcome".to_owned(),
            schema_version: 1,
            protocol: DRIVER_PROCESS_PROTOCOL_V1.to_owned(),
            error,
        }
    }

    pub fn validate(self) -> Result<DriverProcessResult, DriverContractError> {
        match self {
            Self::Succeeded {
                message_type,
                schema_version,
                protocol,
                receipt,
            } => {
                validate_process_envelope(&message_type, schema_version, &protocol)?;
                Ok(DriverProcessResult::Succeeded { receipt })
            }
            Self::Failed {
                message_type,
                schema_version,
                protocol,
                error,
            } => {
                validate_process_envelope(&message_type, schema_version, &protocol)?;
                Ok(DriverProcessResult::Failed { error })
            }
        }
    }
}

fn validate_process_envelope(
    message_type: &str,
    schema_version: u32,
    protocol: &str,
) -> Result<(), DriverContractError> {
    if message_type != "outcome" || schema_version != 1 || protocol != DRIVER_PROCESS_PROTOCOL_V1 {
        return Err(DriverContractError::new(
            "driver process outcome does not use the exact v1 protocol",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum DriverProcessResult {
    Succeeded { receipt: DriverReceipt },
    Failed { error: DriverError },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutableProviderDriverConfig {
    pub credential_lease_duration: Duration,
    pub process_timeout: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl Default for ExecutableProviderDriverConfig {
    fn default() -> Self {
        Self {
            credential_lease_duration: Duration::from_secs(30),
            process_timeout: Duration::from_secs(20),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
        }
    }
}

impl ExecutableProviderDriverConfig {
    pub fn validate(self) -> Result<Self, DriverContractError> {
        if self.process_timeout.is_zero()
            || self.credential_lease_duration <= self.process_timeout
            || self.max_stdout_bytes == 0
            || self.max_stderr_bytes == 0
        {
            return Err(DriverContractError::new(
                "driver process needs non-zero bounds and a credential lease longer than its timeout",
            ));
        }
        Ok(self)
    }
}

pub struct ExecutableProviderDriver {
    manifest: ValidatedProviderDriverManifest,
    installed: InstalledDriverRelease,
    executable: InstalledReleaseExecutable,
    credential_broker: Arc<dyn ProviderCredentialBroker>,
    config: ExecutableProviderDriverConfig,
    runner: PinnedCommandRunner,
}

impl ExecutableProviderDriver {
    pub fn new(
        manifest: ValidatedProviderDriverManifest,
        installed: InstalledDriverRelease,
        credential_broker: Arc<dyn ProviderCredentialBroker>,
        config: ExecutableProviderDriverConfig,
    ) -> Result<Self, DriverContractError> {
        let config = config.validate()?;
        if !manifest
            .get()
            .credential_transport
            .allowed
            .iter()
            .any(|transport| transport == "short-lived-environment")
        {
            return Err(DriverContractError::new(
                "executable driver manifest does not allow the dispatcher credential transport",
            ));
        }
        let mut drivers = installed
            .executables()
            .map_err(|_| {
                DriverContractError::new(
                    "installed driver tree failed verification while selecting its entrypoint",
                )
            })?
            .into_iter()
            .filter(|entry| entry.role == DriverReleaseComponentRole::Driver);
        let executable = drivers.next().ok_or_else(|| {
            DriverContractError::new("installed release has no executable driver entrypoint")
        })?;
        if drivers.next().is_some()
            || executable.name != manifest.get().driver.id
            || executable.executable.version != manifest.get().driver.version
        {
            return Err(DriverContractError::new(
                "installed release driver entrypoint does not match its manifest identity",
            ));
        }
        Ok(Self {
            manifest,
            installed,
            executable,
            credential_broker,
            config,
            runner: PinnedCommandRunner,
        })
    }

    async fn release_failed_lease(
        &self,
        lease: crate::ProviderCredentialLease,
        error: DriverError,
    ) -> DriverError {
        let lease_id = lease.lease_id();
        if let Err(release_error) = self
            .credential_broker
            .release(lease, CredentialLeaseDisposition::Failed)
            .await
        {
            warn!(
                lease_id = %lease_id,
                error = %release_error,
                "Provider driver credential lease release will be reconciled after expiry"
            );
        }
        error
    }
}

#[async_trait]
impl ProviderDriver for ExecutableProviderDriver {
    fn manifest(&self) -> &ValidatedProviderDriverManifest {
        &self.manifest
    }

    async fn verify_before_execution(&self) -> Result<(), DriverContractError> {
        let installed = self.installed.clone();
        tokio::task::spawn_blocking(move || installed.verify())
            .await
            .map_err(|_| {
                DriverContractError::new(
                    "installed provider driver verification task did not complete",
                )
            })?
            .map_err(|_| {
                DriverContractError::new(
                    "installed provider driver tree no longer matches its registered release",
                )
            })
    }

    async fn execute(
        &self,
        context: &DriverExecutionContext<'_>,
    ) -> Result<DriverReceipt, DriverError> {
        let request = context.request();
        let process_request = DriverProcessRequest::new(request.clone());
        let request_json = serde_jcs::to_vec(&process_request).map_err(|_| {
            driver_error(
                request,
                DriverErrorCategory::DriverIntegrity,
                "DRIVER_REQUEST_SERIALIZATION",
                "provider driver request could not be canonicalized",
                false,
                SideEffect::None,
                DriverNextAction::None,
            )
        })?;
        let mut arguments = self.executable.argument_prefix.clone();
        arguments.extend([
            "execute".to_owned(),
            "--protocol".to_owned(),
            DRIVER_PROCESS_PROTOCOL_V1.to_owned(),
        ]);
        let process_policy = side_effect_policy(request.operation);
        let mut invocation =
            CommandInvocation::new(&self.installed.root, arguments, process_policy);
        invocation
            .stdin(request_json)
            .and_then(|invocation| invocation.timeout(self.config.process_timeout))
            .and_then(|invocation| {
                invocation.output_limits(self.config.max_stdout_bytes, self.config.max_stderr_bytes)
            })
            .map_err(|error| command_error(request, &error))?;

        let lease = self
            .credential_broker
            .acquire(context, self.config.credential_lease_duration)
            .await
            .map_err(|error| broker_error(request, &error))?;
        let remaining = lease.expires_at().signed_duration_since(Utc::now());
        let required = chrono::Duration::from_std(self.config.process_timeout)
            .unwrap_or(chrono::Duration::MAX)
            + chrono::Duration::seconds(1);
        if remaining < required {
            let error = driver_error(
                request,
                DriverErrorCategory::Internal,
                "DRIVER_CREDENTIAL_LEASE_TOO_SHORT",
                "credential lease is too short for the bounded driver process execution",
                true,
                SideEffect::None,
                DriverNextAction::Retry,
            );
            return Err(self.release_failed_lease(lease, error).await);
        }
        let credential = match std::str::from_utf8(lease.material().expose_secret()) {
            Ok(credential) => credential.to_owned(),
            Err(_) => {
                let error = driver_error(
                    request,
                    DriverErrorCategory::Configuration,
                    "DRIVER_CREDENTIAL_ENCODING",
                    "executable provider drivers require a UTF-8 service credential",
                    false,
                    SideEffect::None,
                    DriverNextAction::None,
                );
                return Err(self.release_failed_lease(lease, error).await);
            }
        };
        let secret = match SecretString::new(credential) {
            Ok(secret) => secret,
            Err(secret_error) => {
                let error = driver_error(
                    request,
                    DriverErrorCategory::Configuration,
                    "DRIVER_CREDENTIAL_ENCODING",
                    &secret_error.to_string(),
                    false,
                    SideEffect::None,
                    DriverNextAction::None,
                );
                return Err(self.release_failed_lease(lease, error).await);
            }
        };
        let injection = match SecretInjection::environment(DRIVER_PROCESS_CREDENTIAL_ENV, secret) {
            Ok(injection) => injection,
            Err(invocation_error) => {
                let error = command_error(request, &invocation_error);
                return Err(self.release_failed_lease(lease, error).await);
            }
        };
        invocation.credential(injection);

        let output = self
            .runner
            .run(&self.executable.executable, &invocation)
            .await;
        drop(invocation);
        let process_result = output
            .map_err(|error| command_error(request, &error))
            .and_then(|output| {
                serde_json::from_value::<DriverProcessOutcome>(output.structured).map_err(|_| {
                    let possible = process_policy == SideEffectPolicy::MayChangeProvider;
                    driver_error(
                        request,
                        DriverErrorCategory::DriverIntegrity,
                        "DRIVER_PROCESS_PROTOCOL",
                        "provider driver process returned an invalid v1 outcome envelope",
                        false,
                        if possible {
                            SideEffect::Possible
                        } else {
                            SideEffect::None
                        },
                        if possible {
                            DriverNextAction::Reconcile
                        } else {
                            DriverNextAction::None
                        },
                    )
                })
            });
        let process_result = process_result.and_then(|outcome| {
            outcome.validate().map_err(|_| {
                let possible = process_policy == SideEffectPolicy::MayChangeProvider;
                driver_error(
                    request,
                    DriverErrorCategory::DriverIntegrity,
                    "DRIVER_PROCESS_PROTOCOL",
                    "provider driver process returned an outcome for another protocol version",
                    false,
                    if possible {
                        SideEffect::Possible
                    } else {
                        SideEffect::None
                    },
                    if possible {
                        DriverNextAction::Reconcile
                    } else {
                        DriverNextAction::None
                    },
                )
            })
        });
        let disposition = if matches!(process_result, Ok(DriverProcessResult::Succeeded { .. })) {
            CredentialLeaseDisposition::Succeeded
        } else {
            CredentialLeaseDisposition::Failed
        };
        let lease_id = lease.lease_id();
        if let Err(error) = self.credential_broker.release(lease, disposition).await {
            warn!(
                lease_id = %lease_id,
                error = %error,
                "Provider driver credential lease release will be reconciled after expiry"
            );
        }
        match process_result? {
            DriverProcessResult::Succeeded { receipt } => Ok(receipt),
            DriverProcessResult::Failed { error } => Err(error),
        }
    }
}

fn side_effect_policy(operation: DriverOperation) -> SideEffectPolicy {
    match operation {
        DriverOperation::Preflight
        | DriverOperation::Plan
        | DriverOperation::Verify
        | DriverOperation::Observe
        | DriverOperation::Inspect => SideEffectPolicy::ReadOnly,
        DriverOperation::Prepare
        | DriverOperation::Upload
        | DriverOperation::Activate
        | DriverOperation::Rollback
        | DriverOperation::Reconcile
        | DriverOperation::Delete
        | DriverOperation::Cleanup => SideEffectPolicy::MayChangeProvider,
    }
}

fn broker_error(request: &crate::DriverRequest, error: &CredentialBrokerError) -> DriverError {
    let (category, code, retryable) = match error.kind() {
        CredentialBrokerErrorKind::Unavailable | CredentialBrokerErrorKind::Internal => (
            DriverErrorCategory::Internal,
            "DRIVER_CREDENTIAL_UNAVAILABLE",
            true,
        ),
        CredentialBrokerErrorKind::Unauthorized | CredentialBrokerErrorKind::Revoked => (
            DriverErrorCategory::Authentication,
            "DRIVER_CREDENTIAL_DENIED",
            false,
        ),
        CredentialBrokerErrorKind::Integrity => (
            DriverErrorCategory::DriverIntegrity,
            "DRIVER_CREDENTIAL_INTEGRITY",
            false,
        ),
    };
    driver_error(
        request,
        category,
        code,
        &error.to_string(),
        retryable,
        SideEffect::None,
        if retryable {
            DriverNextAction::Retry
        } else {
            DriverNextAction::None
        },
    )
}

fn command_error(request: &crate::DriverRequest, error: &CommandExecutionError) -> DriverError {
    let integrity = matches!(
        error.kind,
        CommandExecutionErrorKind::InvalidPin
            | CommandExecutionErrorKind::IntegrityMismatch
            | CommandExecutionErrorKind::InvalidInvocation
            | CommandExecutionErrorKind::InvalidStructuredOutput
    );
    let possible = error.next_action == DriverNextAction::Reconcile;
    driver_error(
        request,
        if integrity {
            DriverErrorCategory::DriverIntegrity
        } else if possible {
            DriverErrorCategory::UnknownState
        } else {
            DriverErrorCategory::Internal
        },
        if integrity {
            "DRIVER_PROCESS_INTEGRITY"
        } else {
            "DRIVER_PROCESS_EXECUTION"
        },
        &error.message,
        error.next_action == DriverNextAction::Retry,
        if possible {
            SideEffect::Possible
        } else {
            SideEffect::None
        },
        error.next_action,
    )
}

#[allow(clippy::too_many_arguments)]
fn driver_error(
    request: &crate::DriverRequest,
    category: DriverErrorCategory,
    code: &str,
    message: &str,
    retryable: bool,
    side_effect: SideEffect,
    next_action: DriverNextAction,
) -> DriverError {
    DriverError {
        operation_id: request.operation_id,
        operation: request.operation,
        desired_state_digest: request.desired_state_digest.clone(),
        category,
        code: code.to_owned(),
        message: bounded_message(message),
        retryable,
        side_effect,
        next_action,
        provider_request_id: None,
    }
}

fn bounded_message(value: &str) -> String {
    const MAX: usize = 4_096;
    if value.len() <= MAX {
        return value.to_owned();
    }
    let mut end = MAX;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{
        collections::BTreeMap,
        fs::{self, File},
        os::unix::fs::PermissionsExt as _,
        path::Path,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    #[cfg(unix)]
    use crate::{
        CredentialMaterial, DriverReleaseComponent, DriverReleaseFile, DriverReleaseInstaller,
        HostOperatingSystem, ProviderCredentialLease, ProviderDriverManifest,
        ProviderDriverRelease, ValidatedProviderDriverRelease,
    };
    use crate::{
        DeploymentState, DriverStepState, ProviderNeutralPlan, ProviderTargetRef, Sha256Digest,
    };
    #[cfg(unix)]
    use async_trait::async_trait;
    use deis_wire::CanonicalId;
    #[cfg(unix)]
    use serde::Serialize;
    #[cfg(unix)]
    use sha2::{Digest as _, Sha256};

    fn request(operation: DriverOperation) -> crate::DriverRequest {
        crate::DriverRequest {
            schema_version: 1,
            operation_id: CanonicalId::new_v7(),
            operation,
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

    #[test]
    fn outcome_envelope_is_explicit_and_round_trips() {
        let request = request(DriverOperation::Preflight);
        let outcome = DriverProcessOutcome::succeeded(DriverReceipt {
            schema_version: 1,
            operation_id: request.operation_id,
            operation: request.operation,
            desired_state_digest: request.desired_state_digest,
            state: DriverStepState::Succeeded,
            deployment_state: Some(DeploymentState::Planned),
            side_effect: SideEffect::None,
            provider_request_id: Some("provider-request".to_owned()),
            provider_version_id: None,
            provider_deployment_id: None,
            provider_resource_ids: Default::default(),
            observed_at: Utc::now(),
            next_action: DriverNextAction::None,
            diagnostic: None,
        });
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["messageType"], "outcome");
        assert_eq!(json["schemaVersion"], 1);
        assert_eq!(json["protocol"], DRIVER_PROCESS_PROTOCOL_V1);
        assert_eq!(json["outcome"], "succeeded");
        assert_eq!(
            serde_json::from_value::<DriverProcessOutcome>(json).unwrap(),
            outcome
        );
        let fixture: DriverProcessOutcome = serde_json::from_slice(include_bytes!(
            "../../../contracts/v1/fixtures/valid/provider-driver-process.json"
        ))
        .unwrap();
        assert!(fixture.validate().is_ok());
        let mut with_extra = serde_json::to_value(&outcome).unwrap();
        with_extra["vendorExtension"] = serde_json::json!(true);
        assert!(serde_json::from_value::<DriverProcessOutcome>(with_extra).is_err());
    }

    #[test]
    fn request_fixture_uses_the_same_strict_rust_envelope() {
        let fixture: DriverProcessRequest = serde_json::from_slice(include_bytes!(
            "../../../contracts/v1/fixtures/valid/provider-driver-process-request.json"
        ))
        .unwrap();
        assert!(fixture.validate().is_ok());
    }

    #[test]
    fn side_effecting_process_failures_require_reconciliation() {
        let request = request(DriverOperation::Activate);
        let error = CommandExecutionError {
            kind: CommandExecutionErrorKind::Timeout,
            message: "timed out".to_owned(),
            next_action: DriverNextAction::Reconcile,
            diagnostic: None,
        };
        let mapped = command_error(&request, &error);
        assert_eq!(mapped.category, DriverErrorCategory::UnknownState);
        assert_eq!(mapped.side_effect, SideEffect::Possible);
        assert_eq!(mapped.next_action, DriverNextAction::Reconcile);
        assert!(!mapped.retryable);
    }

    #[test]
    fn process_bounds_fit_inside_the_credential_lease() {
        assert!(ExecutableProviderDriverConfig::default().validate().is_ok());
        assert!(
            ExecutableProviderDriverConfig {
                credential_lease_duration: Duration::from_secs(20),
                process_timeout: Duration::from_secs(20),
                ..ExecutableProviderDriverConfig::default()
            }
            .validate()
            .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn installed_process_receives_the_exact_request_and_an_operation_scoped_credential() {
        let request = request(DriverOperation::Preflight);
        let expected_receipt = DriverReceipt {
            schema_version: 1,
            operation_id: request.operation_id,
            operation: request.operation,
            desired_state_digest: request.desired_state_digest.clone(),
            state: DriverStepState::Succeeded,
            deployment_state: Some(DeploymentState::Planned),
            side_effect: SideEffect::None,
            provider_request_id: Some("process-request-1".to_owned()),
            provider_version_id: None,
            provider_deployment_id: None,
            provider_resource_ids: Default::default(),
            observed_at: Utc::now(),
            next_action: DriverNextAction::None,
            diagnostic: None,
        };
        let input = String::from_utf8(
            serde_jcs::to_vec(&DriverProcessRequest::new(request.clone())).unwrap(),
        )
        .unwrap();
        let output = String::from_utf8(
            serde_jcs::to_vec(&DriverProcessOutcome::succeeded(expected_receipt.clone())).unwrap(),
        )
        .unwrap();
        assert!(!input.contains('\''));
        assert!(!output.contains('\''));
        let script = format!(
            "#!/bin/sh\nset -eu\n\
             test \"$1\" = execute\n\
             test \"$2\" = --protocol\n\
             test \"$3\" = {DRIVER_PROCESS_PROTOCOL_V1}\n\
             test \"${{{DRIVER_PROCESS_CREDENTIAL_ENV}}}\" = process-secret\n\
             input=$(/bin/cat)\n\
             test \"$input\" = '{input}'\n\
             printf '%s' '{output}'\n"
        );
        let fixture = ProcessFixture::new(script.into_bytes());
        let installed = DriverReleaseInstaller::new(&fixture.install_root)
            .unwrap()
            .install(
                &fixture.release,
                &fixture.manifest,
                HostOperatingSystem::current().unwrap(),
                &fixture.archive,
            )
            .unwrap();
        let broker = Arc::new(TestBroker::default());
        let broker_trait: Arc<dyn ProviderCredentialBroker> = broker.clone();
        let driver = ExecutableProviderDriver::new(
            fixture.manifest.clone(),
            installed,
            broker_trait,
            ExecutableProviderDriverConfig {
                credential_lease_duration: Duration::from_secs(10),
                process_timeout: Duration::from_secs(5),
                ..ExecutableProviderDriverConfig::default()
            },
        )
        .unwrap();
        let context = DriverExecutionContext::new(&request, CanonicalId::new_v7(), 1);
        let receipt = driver.execute(&context).await.unwrap();
        assert_eq!(receipt, expected_receipt);
        assert_eq!(broker.acquired.load(Ordering::SeqCst), 1);
        assert_eq!(
            broker.released.lock().unwrap().as_slice(),
            [CredentialLeaseDisposition::Succeeded]
        );
        make_writable_for_cleanup(fixture.temporary.path());
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct TestBroker {
        acquired: AtomicUsize,
        released: Mutex<Vec<CredentialLeaseDisposition>>,
    }

    #[cfg(unix)]
    #[async_trait]
    impl ProviderCredentialBroker for TestBroker {
        async fn acquire(
            &self,
            context: &DriverExecutionContext<'_>,
            _requested_duration: Duration,
        ) -> Result<ProviderCredentialLease, CredentialBrokerError> {
            self.acquired.fetch_add(1, Ordering::SeqCst);
            Ok(ProviderCredentialLease::new(
                CanonicalId::new_v7(),
                CanonicalId::new_v7(),
                1,
                context.request().operation_id,
                context.owner_id(),
                context.claim_epoch(),
                Utc::now() + chrono::Duration::seconds(30),
                CredentialMaterial::new(b"process-secret".to_vec()).unwrap(),
            ))
        }

        async fn release(
            &self,
            _lease: ProviderCredentialLease,
            disposition: CredentialLeaseDisposition,
        ) -> Result<(), CredentialBrokerError> {
            self.released.lock().unwrap().push(disposition);
            Ok(())
        }
    }

    #[cfg(unix)]
    struct ProcessFixture {
        temporary: tempfile::TempDir,
        install_root: std::path::PathBuf,
        archive: std::path::PathBuf,
        release: ValidatedProviderDriverRelease,
        manifest: ValidatedProviderDriverManifest,
    }

    #[cfg(unix)]
    impl ProcessFixture {
        fn new(driver: Vec<u8>) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let install_root = temporary.path().join("installed");
            fs::create_dir(&install_root).unwrap();
            fs::set_permissions(&install_root, fs::Permissions::from_mode(0o700)).unwrap();
            let archive = temporary.path().join("driver.tar");
            let host = HostOperatingSystem::current().unwrap();
            let template: ProviderDriverRelease = serde_json::from_slice(include_bytes!(
                "../../../contracts/v1/fixtures/valid/provider-driver-release.json"
            ))
            .unwrap();
            let mut distribution = template
                .distributions
                .iter()
                .find(|distribution| distribution.host_operating_system == host)
                .unwrap()
                .clone();
            let contents = distribution
                .files
                .iter()
                .map(|file| {
                    (
                        file.path.clone(),
                        if file.path.contains("driver") {
                            driver.clone()
                        } else {
                            b"#!/bin/sh\nexit 0\n".to_vec()
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>();
            bind_file_table(&mut distribution.files, &contents);
            bind_components(&mut distribution.components, &distribution.files);
            distribution.tree_digest.value = digest_jcs(&distribution.files);
            write_archive(&archive, &contents);
            let archive_bytes = fs::read(&archive).unwrap();
            distribution.archive.size_bytes = archive_bytes.len() as u64;
            distribution.archive.sha256.value = digest_bytes(&archive_bytes);
            let mut release = template;
            release.distributions = vec![distribution];
            let release = ValidatedProviderDriverRelease::new(release).unwrap();
            let mut manifest: ProviderDriverManifest = serde_json::from_slice(include_bytes!(
                "../../../contracts/v1/fixtures/valid/provider-driver.json"
            ))
            .unwrap();
            manifest.supported_host_operating_systems = vec![host.as_str().to_owned()];
            manifest.driver.distribution_digest.value = release.canonical_sha256().unwrap();
            let manifest = ValidatedProviderDriverManifest::new(manifest).unwrap();
            Self {
                temporary,
                install_root,
                archive,
                release,
                manifest,
            }
        }
    }

    #[cfg(unix)]
    fn digest_bytes(bytes: &[u8]) -> Sha256Digest {
        hex::encode(Sha256::digest(bytes)).parse().unwrap()
    }

    #[cfg(unix)]
    fn digest_jcs(value: &impl Serialize) -> Sha256Digest {
        digest_bytes(&serde_jcs::to_vec(value).unwrap())
    }

    #[cfg(unix)]
    fn bind_file_table(files: &mut [DriverReleaseFile], contents: &BTreeMap<String, Vec<u8>>) {
        for file in files {
            let content = &contents[&file.path];
            file.size_bytes = content.len() as u64;
            file.sha256.value = digest_bytes(content);
        }
    }

    #[cfg(unix)]
    fn bind_components(components: &mut [DriverReleaseComponent], files: &[DriverReleaseFile]) {
        for component in components {
            let component_files = component
                .file_paths
                .as_ref()
                .unwrap()
                .iter()
                .map(|path| files.iter().find(|file| &file.path == path).unwrap())
                .collect::<Vec<_>>();
            component.materialization_digest.value = digest_jcs(&component_files);
            let executable = component.executable.as_mut().unwrap();
            executable.sha256 = files
                .iter()
                .find(|file| file.path == executable.path)
                .unwrap()
                .sha256
                .clone();
        }
    }

    #[cfg(unix)]
    fn write_archive(path: &Path, contents: &BTreeMap<String, Vec<u8>>) {
        let file = File::create(path).unwrap();
        let mut builder = tar::Builder::new(file);
        for (path, content) in contents {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o755);
            header.set_size(content.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, path, content.as_slice())
                .unwrap();
        }
        builder.finish().unwrap();
        builder.into_inner().unwrap().sync_all().unwrap();
    }

    #[cfg(unix)]
    fn make_writable_for_cleanup(root: &Path) {
        let Ok(metadata) = fs::symlink_metadata(root) else {
            return;
        };
        if metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && let Ok(entries) = fs::read_dir(root)
        {
            let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o700));
            for entry in entries.flatten() {
                make_writable_for_cleanup(&entry.path());
            }
        } else if metadata.is_file() {
            let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o600));
        }
    }
}
