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
mod release_transparency;
#[cfg(windows)]
mod windows_security;

#[cfg(all(windows, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support {
    use std::{io, path::Path};

    /// Builds explicit Windows ACL fixtures for cross-crate security tests.
    ///
    /// This API exists only behind the `test-support` feature. Production code
    /// must provision the service-owned directory and trust files outside the
    /// running Control process, then let the normal constructors validate them.
    pub fn set_windows_private_acl(
        path: &Path,
        directory: bool,
        add_untrusted_writer: bool,
        protected: bool,
    ) -> io::Result<()> {
        crate::windows_security::set_private_acl_for_test(
            path,
            directory,
            add_untrusted_writer,
            protected,
        )
        .map_err(|error| match error {
            crate::windows_security::WindowsSecurityError::Io(error) => error,
            crate::windows_security::WindowsSecurityError::Insecure(message) => {
                io::Error::other(message)
            }
        })
    }
}

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
    DriverNextAction, DriverOperation, DriverReceipt, DriverRequest, DriverRollbackTarget,
    DriverStepState, ProviderNeutralPlan, ProviderTargetRef, SideEffect,
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
    DSSE_ENVELOPE_PAYLOAD_TYPE, DSSE_TRUST_ROOT_PAYLOAD_TYPE, DriverReleaseAuthorization,
    DriverReleaseAuthorizationError, DriverReleaseAuthorizationSubject, DriverReleaseDsseEnvelope,
    DriverReleaseDsseSignature, DriverReleaseRootRole, DriverReleaseSigningKey,
    DriverReleaseSigningKeyStatus, DriverReleaseSigningRole, DriverReleaseTrustRoot,
    DriverTrustRootDsseEnvelope, ValidatedDriverReleaseAuthorization,
    ValidatedDriverReleaseTrustRoot, ValidatedDriverTrustRootRotation,
    VerifiedDriverReleaseAuthorization, VerifiedDriverTrustRootRotation,
};
pub use release_transparency::{
    DriverReleaseTransparencyEntry, DriverReleaseTransparencyLog, DriverReleaseTransparencyPolicy,
    DriverReleaseTransparencyProof, DriverReleaseTransparencyWitness,
    TRANSPARENCY_PROOF_MEDIA_TYPE, ValidatedDriverReleaseTransparencyProof,
    VerifiedDriverReleaseTransparencyProof,
};
