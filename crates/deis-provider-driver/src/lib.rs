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
mod installer;
mod lifecycle;
mod manifest;
mod model;
mod orchestrator;
mod process;
mod release;
mod release_authorization;

pub use credential::{
    CredentialBrokerError, CredentialBrokerErrorKind, CredentialLeaseDisposition,
    CredentialMaterial, CredentialMaterialError, ProviderCredentialBroker, ProviderCredentialLease,
};
pub use digest::{Sha256Digest, Sha256DigestError};
pub use executor::{
    CommandExecutionError, CommandExecutionErrorKind, CommandInvocation, CommandOutput,
    PinnedCommandRunner, PinnedExecutable, SecretInjection, SecretString, SideEffectPolicy,
};
pub use installer::{
    DriverInstallationError, DriverReleaseInstaller, InstalledDriverRelease,
    InstalledReleaseExecutable,
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
pub use process::{
    DRIVER_PROCESS_CREDENTIAL_ENV, DRIVER_PROCESS_PROTOCOL_V1, DriverProcessOutcome,
    DriverProcessRequest, DriverProcessResult, ExecutableProviderDriver,
    ExecutableProviderDriverConfig,
};
pub use release::{
    DriverHostDistribution, DriverReleaseArchive, DriverReleaseComponent,
    DriverReleaseComponentRole, DriverReleaseError, DriverReleaseExecutable, DriverReleaseFile,
    DriverReleaseIdentity, DriverReleaseImplementation, DriverReleaseInstallationPolicy,
    HostOperatingSystem, ProviderDriverRelease, ValidatedProviderDriverRelease,
};
pub use release_authorization::{
    DSSE_ENVELOPE_PAYLOAD_TYPE, DriverReleaseAuthorization, DriverReleaseAuthorizationError,
    DriverReleaseAuthorizationSubject, DriverReleaseDsseEnvelope, DriverReleaseDsseSignature,
    DriverReleaseSigningKey, DriverReleaseSigningKeyStatus, DriverReleaseSigningRole,
    DriverReleaseTrustRoot, ValidatedDriverReleaseAuthorization, ValidatedDriverReleaseTrustRoot,
    VerifiedDriverReleaseAuthorization,
};
