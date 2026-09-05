//! Provider-neutral deployment driver contracts.
//!
//! Build adapters translate one canonical artifact into a provider payload.
//! This crate starts at the next trust boundary: it gives every backend the
//! same durable lifecycle and provides the only supported way to execute a
//! pinned official CLI. Provider credentials are deliberately absent from all
//! serializable request and receipt types.

mod credential;
mod digest;
mod executor;
mod lifecycle;
mod manifest;
mod model;
mod orchestrator;

pub use credential::{
    CredentialBrokerError, CredentialBrokerErrorKind, CredentialLeaseDisposition,
    CredentialMaterial, CredentialMaterialError, ProviderCredentialBroker, ProviderCredentialLease,
};
pub use digest::{Sha256Digest, Sha256DigestError};
pub use executor::{
    CommandExecutionError, CommandExecutionErrorKind, CommandInvocation, CommandOutput,
    PinnedCommandRunner, PinnedExecutable, SecretInjection, SecretString, SideEffectPolicy,
};
pub use lifecycle::{
    DriverContractError, DriverExecutionContext, ProviderDriver, validate_driver_error,
    validate_driver_request, validate_driver_response,
};
pub use manifest::{
    BackendManifest, ClaimPolicy, CredentialPolicy, DriverIdentity, DriverManifestError,
    ExecutionPolicy, ImplementationKind, ImplementationMapping, OfficialTool, OfficialToolKind,
    ProviderDriverManifest, SupportedStandard, ToolDigest, ValidatedProviderDriverManifest,
};
pub use model::{
    ALL_DRIVER_OPERATIONS, ArtifactVersionRef, DeploymentState, DriverError, DriverErrorCategory,
    DriverNextAction, DriverOperation, DriverReceipt, DriverRequest, DriverStepState,
    ProviderNeutralPlan, ProviderTargetRef, SideEffect,
};
pub use orchestrator::{
    DriverOperationClaim, DriverOperationClaimOutcome, DriverOperationStore,
    DriverOrchestrationOutcome, DriverOrchestrator, DriverOrchestratorConfig,
    DriverOrchestratorConfigError, DriverOrchestratorError, ProviderDriverPin,
};
