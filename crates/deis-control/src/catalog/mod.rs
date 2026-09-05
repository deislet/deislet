//! The catalog: the authoritative record of which applications exist,
//! which versions each of them has, and where each environment points,
//! plus the routes and secrets those environments need.
//!
//! The name is the definition. "registry" was considered and rejected:
//! in this field it reads as an artifact registry, which is what the
//! bundle store next door is, not what this is. The Chinese specification
//! calls the same thing the product model
//! (docs/implementation-plan.zh.md section 3); its SQL is exactly these
//! tables.
//!
//! This is the metadata the platform is actually about, and the control
//! plane is its only authority. The node-management tables next door
//! (nodes / configs / heartbeats / commands / events) describe *processes*;
//! everything in here describes *what those processes should be running*.
//!
//! Two invariants are load bearing and are enforced by this module rather
//! than by convention:
//!
//! * **Versions are append-only.** A version row is written once, when a
//!   bundle is uploaded, and never updated. There is deliberately no
//!   update or delete entry point for a single version; the only thing
//!   that removes version rows is deleting the whole app.
//! * **A deployment is a pointer, not a state machine.** The current
//!   version of an environment is simply the newest deployment row for
//!   that environment. Rolling back inserts another row pointing at an
//!   older version, so the history stays complete and a rollback is as
//!   cheap as a deploy.

pub mod backend_migration;
pub mod backend_restore;
pub mod backend_restore_driver;
pub mod bundle;
pub mod compatibility;
pub mod domains;
pub mod identity_migration;
pub mod limits;
pub mod model;
pub mod operation;
pub mod secrets;
pub mod store;

pub use backend_migration::{
    BackendAttemptState, BackendMigrationAttempt, BackendMigrationEvidence,
    IdentityMigrationCoordinator, IdentityMigrationRun, MigrationCoordinatorError,
    summarize_identity_migration,
};
pub use bundle::{BundleStore, StoredBundle};
pub use compatibility::{
    CompatibilityProfile, DeclaredCompatibility, declared_compatibility, describe_version,
};
pub use deis_bundle::Manifest as BundleManifest;
pub use domains::{ClaimRefusal, DomainGrant, GrantPattern, HostClaim};
pub use identity_migration::{
    CatalogBindingInventory, CatalogDeploymentInventory, CatalogEnvironmentInventory,
    CatalogIdentityInventory, CatalogInventoryDigest, CatalogRouteInventory,
    CatalogSecretInventory, CatalogVersionInventory, ComponentInventoryEvidence,
    IdentityMigrationPlan, IdentityMigrationState, InventoryEvidenceState,
};
pub use limits::{DeclaredLimits, declared_limits};
pub use model::{
    App, Assignment, DeletedApp, DeployedVersion, Deployment, DeploymentDrainObservation,
    DeploymentEvent, DeploymentGenerationDrain, DeploymentPlan, DeploymentProxyObservation,
    DeploymentTargetObservation, Environment, RetiredAppName, Route, SecretMeta, Version, now_secs,
};
pub use operation::OperationRequestContext;
pub use secrets::{SecretCipher, SecretsBackend};
pub use store::{BundlePathRepair, CatalogStore, DeploymentPlanSpec, SecretHeadChange};

use thiserror::Error;

/// Everything the catalog can refuse to do.
///
/// The variants map one-to-one onto HTTP status classes so the REST layer
/// never has to guess: a caller mistake is `Invalid`, a clash with existing
/// state is `Conflict`, and a missing master key is `SecretsUnavailable`
/// (503, never a silent plaintext write).
#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("{0} not found")]
    NotFound(String),

    #[error("{0}")]
    Conflict(String),

    /// A state-changing request named an old resource/operation revision.
    /// This is distinct from a domain conflict so the HTTP layer can return
    /// 412 for an `If-Match` failure without inspecting an error string.
    #[error("{0}")]
    RevisionConflict(String),

    #[error("{0}")]
    Invalid(String),

    /// The caller is who they say they are and is not entitled to what they
    /// asked for. Distinct from `Invalid`, which is a request nobody could
    /// make: this one is a request somebody else could. The only thing that
    /// raises it today is a hostname claim reaching past what the
    /// installation granted; see [`domains`].
    #[error("{0}")]
    Forbidden(String),

    #[error("bundle storage failed: {0}")]
    BundleStorage(String),

    #[error("secret storage is unavailable: {0}")]
    SecretsUnavailable(String),

    #[error("{0}")]
    Database(#[from] rusqlite::Error),

    /// The blocking database task did not finish at all: it panicked, or
    /// the runtime went away under it. Distinct from `Database` because
    /// there is no SQLite answer to report, only the fact that the query
    /// never completed.
    #[error("{0}")]
    Backend(#[from] crate::database::DatabaseError),
}

impl CatalogError {
    pub fn not_found(what: impl Into<String>) -> Self {
        CatalogError::NotFound(what.into())
    }

    pub fn conflict(what: impl Into<String>) -> Self {
        CatalogError::Conflict(what.into())
    }

    pub fn revision_conflict(what: impl Into<String>) -> Self {
        CatalogError::RevisionConflict(what.into())
    }

    pub fn invalid(what: impl Into<String>) -> Self {
        CatalogError::Invalid(what.into())
    }

    pub fn forbidden(what: impl Into<String>) -> Self {
        CatalogError::Forbidden(what.into())
    }
}

pub type CatalogResult<T> = Result<T, CatalogError>;
pub use backend_restore::{BackendRestorePlan, BackendRestoreState, ProductInstallation};
pub use backend_restore_driver::{BackendRestoreCoordinator, BackendRestoreCoordinatorError};
