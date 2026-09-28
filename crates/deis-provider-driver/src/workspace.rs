//! How a provider payload reaches a driver process.
//!
//! A [`crate::DriverRequest`] names its artifact only by digest: it is the
//! durable, fingerprinted record of what the Control asked for, and a path on
//! one host has no place in it. The bytes travel beside it instead. Before
//! every driver process starts, the launcher opens an [`OperationWorkspace`]:
//!
//! - `directory` is a fresh directory, mode `0700`, owned by the Control and
//!   used as the process's working directory. The driver may write scratch
//!   files into it. It is removed when the process has exited, whatever the
//!   outcome.
//! - `artifactDirectory` is `<directory>/<providerPayloadSha256>`: the exact
//!   provider payload tree, copied out of the Control's payload store by a
//!   [`DriverArtifactSource`] and then re-hashed by the launcher. A tree whose
//!   file-set digest is not `providerPayloadSha256` never reaches a driver.
//!   It is `null` exactly when the request carries no artifact.
//! - `stateDirectory` persists across operations for one provider target and
//!   environment, mode `0700`, so a driver can keep a journal of what it
//!   created between `prepare` and `cleanup`. It never holds credentials.
//!
//! The process environment is cleared. Besides the operation-scoped
//! credential, the launcher sets exactly [`DRIVER_PROCESS_PATH`] as `PATH`,
//! `TMPDIR` to `directory`, and `HOME` to a private (`0700`) directory shared
//! by every driver process of this Control, where a driver may cache the
//! toolchain it installs. No other variable of the Control reaches a driver.
//!
//! The three paths are carried in the process envelope
//! ([`crate::DriverProcessRequest::workspace`]) and described by
//! `contracts/v1/provider-driver-process.schema.json`.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use deis_wire::CanonicalId;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{ArtifactVersionRef, DriverContractError, DriverRequest};

const OPERATIONS_DIRECTORY: &str = "operations";
const STATE_DIRECTORY: &str = "state";
const HOME_DIRECTORY: &str = "home";

/// The fixed `PATH` a driver process gets.
pub const DRIVER_PROCESS_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// The files one driver process may use, as the process envelope names them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DriverWorkspace {
    /// Per-operation scratch directory and the process's working directory.
    pub directory: PathBuf,
    /// The verified provider payload, `<directory>/<providerPayloadSha256>`.
    pub artifact_directory: Option<PathBuf>,
    /// Durable per-target, per-environment driver state.
    pub state_directory: PathBuf,
}

impl DriverWorkspace {
    /// Check the envelope's paths agree with the request they accompany.
    ///
    /// A driver runs this before touching the file system: every path must be
    /// absolute, the artifact directory must be present exactly when the
    /// request names an artifact and must be the payload digest directly
    /// beneath the operation directory, and the state directory must not live
    /// inside a directory that is deleted after the operation.
    pub fn validate_for(&self, request: &DriverRequest) -> Result<(), DriverContractError> {
        if !self.directory.is_absolute() || !self.state_directory.is_absolute() {
            return Err(DriverContractError::new(
                "driver workspace paths must be absolute",
            ));
        }
        if self.state_directory.starts_with(&self.directory) {
            return Err(DriverContractError::new(
                "driver state directory must outlive the operation directory",
            ));
        }
        match (&request.artifact, &self.artifact_directory) {
            (None, None) => Ok(()),
            (Some(artifact), Some(path)) => {
                if *path
                    == self
                        .directory
                        .join(artifact.provider_payload_sha256.as_str())
                {
                    Ok(())
                } else {
                    Err(DriverContractError::new(
                        "driver artifact directory must be the provider payload digest beneath the operation directory",
                    ))
                }
            }
            (Some(_), None) => Err(DriverContractError::new(
                "driver request names an artifact but the workspace carries none",
            )),
            (None, Some(_)) => Err(DriverContractError::new(
                "driver workspace carries an artifact the request does not name",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverArtifactErrorKind {
    /// The payload store holds no payload with this identity.
    NotFound,
    /// The store answered with bytes that are not the named payload.
    Integrity,
    /// The store or the workspace could not be read or written right now.
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverArtifactError {
    pub kind: DriverArtifactErrorKind,
    pub message: String,
}

impl DriverArtifactError {
    #[must_use]
    pub fn new(kind: DriverArtifactErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for DriverArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DriverArtifactError {}

/// Where the launcher copies provider payloads from.
///
/// The Control implements this over its content-addressed payload store. An
/// implementation only copies: the launcher re-hashes the destination tree
/// against `artifact.provider_payload_sha256` before any process sees it.
#[async_trait]
pub trait DriverArtifactSource: Send + Sync {
    /// Copy every file of the payload `artifact` names, for `application_id`,
    /// into `destination`, which exists and is empty.
    async fn materialize(
        &self,
        application_id: CanonicalId,
        artifact: &ArtifactVersionRef,
        destination: &Path,
    ) -> Result<(), DriverArtifactError>;
}

/// The directory every driver workspace of one Control lives under.
#[derive(Clone)]
pub struct DriverWorkspaceRoot {
    root: PathBuf,
    source: Arc<dyn DriverArtifactSource>,
}

impl fmt::Debug for DriverWorkspaceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DriverWorkspaceRoot")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl DriverWorkspaceRoot {
    /// Prepare `root` (absolute) with private `operations/`, `state/` and
    /// `home/` children.
    ///
    /// Operation directories left behind by a Control that stopped mid-run
    /// are removed here: only one Control process owns a root, and nothing
    /// in them outlives the process that was using them.
    pub fn new(
        root: impl Into<PathBuf>,
        source: Arc<dyn DriverArtifactSource>,
    ) -> io::Result<Self> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "driver workspace root must be absolute",
            ));
        }
        create_private_dir_all(&root)?;
        let operations = root.join(OPERATIONS_DIRECTORY);
        create_private_dir_all(&operations)?;
        for entry in fs::read_dir(&operations)? {
            let path = entry?.path();
            if let Err(error) = fs::remove_dir_all(&path) {
                warn!(path = %path.display(), %error, "Could not remove a stale driver operation workspace");
            }
        }
        create_private_dir_all(&root.join(STATE_DIRECTORY))?;
        create_private_dir_all(&root.join(HOME_DIRECTORY))?;
        Ok(Self { root, source })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `HOME` every driver process of this Control gets.
    #[must_use]
    pub fn home(&self) -> PathBuf {
        self.root.join(HOME_DIRECTORY)
    }

    /// Open the workspace one driver process of `request` runs in.
    pub async fn open(
        &self,
        request: &DriverRequest,
    ) -> Result<OperationWorkspace, DriverArtifactError> {
        let directory = self.root.join(OPERATIONS_DIRECTORY).join(format!(
            "{}-{}",
            request.operation_id,
            CanonicalId::new_v7()
        ));
        let state_directory = self
            .root
            .join(STATE_DIRECTORY)
            .join(request.target.provider_target_id.to_string())
            .join(request.environment_id.to_string());
        let unavailable = |action: &str, error: io::Error| {
            DriverArtifactError::new(
                DriverArtifactErrorKind::Unavailable,
                format!("driver workspace could not {action}: {error}"),
            )
        };
        create_private_dir(&directory).map_err(|error| unavailable("be created", error))?;
        // From here on the guard removes the directory on every exit path.
        let mut workspace = OperationWorkspace {
            workspace: DriverWorkspace {
                directory: directory.clone(),
                artifact_directory: None,
                state_directory: state_directory.clone(),
            },
        };
        create_private_dir_all(&state_directory)
            .map_err(|error| unavailable("create its state directory", error))?;
        if let Some(artifact) = &request.artifact {
            let expected = artifact.provider_payload_sha256.as_str().to_owned();
            let artifact_directory = directory.join(&expected);
            create_private_dir(&artifact_directory)
                .map_err(|error| unavailable("create its artifact directory", error))?;
            self.source
                .materialize(request.application_id, artifact, &artifact_directory)
                .await?;
            let verified = artifact_directory.clone();
            tokio::task::spawn_blocking(move || {
                deis_artifact::provider_derivation::validate_payload(&verified, Some(&expected))
            })
            .await
            .map_err(|error| {
                DriverArtifactError::new(
                    DriverArtifactErrorKind::Unavailable,
                    format!("provider payload verification task did not complete: {error}"),
                )
            })?
            .map_err(|error| {
                DriverArtifactError::new(
                    DriverArtifactErrorKind::Integrity,
                    format!(
                        "materialized provider payload does not match providerPayloadSha256: {error}"
                    ),
                )
            })?;
            workspace.workspace.artifact_directory = Some(artifact_directory);
        }
        Ok(workspace)
    }
}

/// One process's workspace; dropping it removes the operation directory.
#[derive(Debug)]
pub struct OperationWorkspace {
    workspace: DriverWorkspace,
}

impl OperationWorkspace {
    #[must_use]
    pub fn get(&self) -> &DriverWorkspace {
        &self.workspace
    }
}

impl Drop for OperationWorkspace {
    fn drop(&mut self) {
        let directory = &self.workspace.directory;
        if let Err(error) = fs::remove_dir_all(directory)
            && error.kind() != io::ErrorKind::NotFound
        {
            warn!(
                path = %directory.display(),
                %error,
                "Could not remove a driver operation workspace; it is removed when the Control next starts"
            );
        }
    }
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Create `path` with mode `0700` (or tighten an existing one). Missing
/// parents are created with the process defaults: the private leaf is what
/// protects its contents, and an operator's existing directories are never
/// re-permissioned.
fn create_private_dir_all(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    match create_private_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {
            restrict(path)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a plain directory", path.display()),
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a plain directory", path.display()),
        ));
    }
    Ok(())
}
