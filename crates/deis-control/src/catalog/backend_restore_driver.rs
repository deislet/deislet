//! Control-side coordinator for the out-of-process backend topology driver.
//!
//! Restore input crosses stdin, never argv or an environment variable. The
//! executable and its config are absolute, stdout is bounded and parsed as a
//! shared report, and an exit/timeout is an unknown outcome: Control leaves
//! the durable operation waiting so the same driver journal can reconcile it.
//!
//! The report is the evidence. The driver is pinned by digest in the restore
//! set, it restarts the three services itself, and it reads each restored
//! directory's backend incarnation from disk before it reports; Control
//! checks that the report binds to the immutable restore set and commits it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use deis_wire::{
    BackendRestoreExecutionReport, BackendRestoreExecutionState, CanonicalId, ProductOperation,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::warn;

use super::{BackendRestorePlan, CatalogError, CatalogStore};
use crate::config::ControlConfig;

const MAX_DRIVER_OUTPUT_BYTES: u64 = 1024 * 1024;
const COORDINATOR_LEASE_SECONDS: u64 = 30;
const COORDINATOR_HEARTBEAT_SECONDS: u64 = 10;

#[derive(Debug, Error)]
pub enum BackendRestoreCoordinatorError {
    #[error("{0}")]
    Configuration(String),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("topology driver outcome is unknown: {0}")]
    Unknown(String),
    #[error("topology driver returned invalid evidence: {0}")]
    Protocol(String),
}

pub struct BackendRestoreCoordinator {
    catalog: Arc<CatalogStore>,
    executable: PathBuf,
    driver_config: PathBuf,
    timeout: Duration,
    coordinator_id: CanonicalId,
}

impl BackendRestoreCoordinator {
    pub fn from_config(
        catalog: Arc<CatalogStore>,
        config: &ControlConfig,
    ) -> Result<Self, BackendRestoreCoordinatorError> {
        let executable = paired_absolute_path(
            config.backend_restore_driver_executable.as_deref(),
            config.backend_restore_driver_config.as_deref(),
            true,
        )?;
        let driver_config = paired_absolute_path(
            config.backend_restore_driver_config.as_deref(),
            config.backend_restore_driver_executable.as_deref(),
            false,
        )?;
        if executable == driver_config {
            return Err(BackendRestoreCoordinatorError::Configuration(
                "backend restore driver executable and config must be distinct files".to_string(),
            ));
        }
        if config.backend_restore_driver_timeout_secs == 0
            || config.backend_restore_driver_timeout_secs > 3600
        {
            return Err(BackendRestoreCoordinatorError::Configuration(
                "backend_restore_driver_timeout_secs must be 1..=3600".to_string(),
            ));
        }
        Ok(Self {
            catalog,
            executable,
            driver_config,
            timeout: Duration::from_secs(config.backend_restore_driver_timeout_secs),
            coordinator_id: CanonicalId::new_v7(),
        })
    }

    pub async fn coordinate(
        &self,
        operation_id: CanonicalId,
    ) -> Result<BackendRestoreExecutionReport, BackendRestoreCoordinatorError> {
        let (plan, operation) = self
            .catalog
            .begin_backend_restore(operation_id, self.coordinator_id, COORDINATOR_LEASE_SECONDS)
            .await?;
        let (stop_sender, mut stop_receiver) = tokio::sync::oneshot::channel();
        let catalog = Arc::clone(&self.catalog);
        let coordinator_id = self.coordinator_id;
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_receiver => break,
                    _ = tokio::time::sleep(Duration::from_secs(COORDINATOR_HEARTBEAT_SECONDS)) => {
                        if let Err(error) = catalog
                            .renew_backend_restore_lease(
                                operation_id,
                                coordinator_id,
                                COORDINATOR_LEASE_SECONDS,
                            )
                            .await
                        {
                            warn!(
                                %operation_id,
                                %coordinator_id,
                                %error,
                                "Backend restore coordinator lost its durable lease; terminal commit will fail closed"
                            );
                            break;
                        }
                    }
                }
            }
        });
        let result = self.coordinate_claimed(plan, operation).await;
        let _ = stop_sender.send(());
        let _ = heartbeat.await;
        result
    }

    async fn coordinate_claimed(
        &self,
        plan: BackendRestorePlan,
        operation: ProductOperation,
    ) -> Result<BackendRestoreExecutionReport, BackendRestoreCoordinatorError> {
        let operation_id = operation.operation_id;
        let expected_driver_digest = &plan.restore_set.topology_driver.distribution_digest.value;
        let actual_driver_digest = match file_sha256(&self.executable) {
            Ok(digest) => digest,
            Err(error) => {
                let reason = "configured topology driver could not be hashed";
                self.defer(operation_id, operation.revision, reason).await?;
                return Err(BackendRestoreCoordinatorError::Configuration(format!(
                    "cannot hash topology driver {}: {error}",
                    self.executable.display()
                )));
            }
        };
        if &actual_driver_digest != expected_driver_digest {
            let reason = "configured topology driver bytes differ from the restore set pin";
            self.defer(operation_id, operation.revision, reason).await?;
            return Err(BackendRestoreCoordinatorError::Configuration(
                reason.to_string(),
            ));
        }
        let input = match plan.restore_set.canonical_json() {
            Ok(input) => input,
            Err(error) => {
                self.defer(
                    operation_id,
                    operation.revision,
                    "durable restore input failed canonical serialization",
                )
                .await?;
                return Err(BackendRestoreCoordinatorError::Protocol(format!(
                    "could not serialize durable restore set: {error}"
                )));
            }
        };
        let invocation = self.run_driver(operation_id, input).await;
        let report = match invocation {
            Ok(output) => match serde_json::from_slice(&output) {
                Ok(report) => report,
                Err(_) => match self.inspect_driver(operation_id).await {
                    Ok(Some(report)) => report,
                    Ok(None) | Err(_) => {
                        self.defer(
                            operation_id,
                            operation.revision,
                            "topology driver stdout was invalid and its journal has no verified terminal state",
                        )
                        .await?;
                        return Err(BackendRestoreCoordinatorError::Protocol(
                            "topology driver stdout was not one valid restore execution report"
                                .to_string(),
                        ));
                    }
                },
            },
            Err(invocation_error) => match self.inspect_driver(operation_id).await {
                Ok(Some(report)) => report,
                Ok(None) | Err(_) => {
                    self.defer(
                        operation_id,
                        operation.revision,
                        "topology driver exited or timed out and its journal has no verified terminal state",
                    )
                    .await?;
                    return Err(invocation_error);
                }
            },
        };
        self.accept_terminal_report(operation_id, report, operation.revision)
            .await
    }

    async fn accept_terminal_report(
        &self,
        claimed_operation_id: CanonicalId,
        report: BackendRestoreExecutionReport,
        operation_revision: u64,
    ) -> Result<BackendRestoreExecutionReport, BackendRestoreCoordinatorError> {
        if report.operation_id != claimed_operation_id {
            self.defer(
                claimed_operation_id,
                operation_revision,
                "topology driver report named an operation other than the claimed restore",
            )
            .await?;
            return Err(BackendRestoreCoordinatorError::Protocol(format!(
                "topology driver report named operation {}, expected {claimed_operation_id}",
                report.operation_id
            )));
        }
        if let Err(error) = self
            .catalog
            .validate_backend_restore_execution_report(&report)
            .await
        {
            self.defer(
                report.operation_id,
                operation_revision,
                "topology driver returned evidence that does not bind to the immutable restore set",
            )
            .await?;
            return Err(BackendRestoreCoordinatorError::Protocol(error.to_string()));
        }
        match report.state {
            BackendRestoreExecutionState::Succeeded => {
                self.catalog
                    .complete_backend_restore(
                        report.clone(),
                        self.coordinator_id,
                        operation_revision,
                    )
                    .await?;
                Ok(report)
            }
            BackendRestoreExecutionState::RolledBack => {
                self.catalog
                    .fail_backend_restore_rolled_back(
                        report,
                        self.coordinator_id,
                        operation_revision,
                        "topology driver proved that a failed restore returned every backend to its prior incarnation"
                            .to_string(),
                    )
                    .await?;
                Err(BackendRestoreCoordinatorError::Protocol(
                    "topology driver rolled back a failed restore".to_string(),
                ))
            }
        }
    }

    async fn defer(
        &self,
        operation_id: CanonicalId,
        operation_revision: u64,
        reason: &str,
    ) -> Result<(), BackendRestoreCoordinatorError> {
        self.catalog
            .defer_backend_restore(
                operation_id,
                self.coordinator_id,
                operation_revision,
                reason.to_string(),
            )
            .await?;
        Ok(())
    }

    async fn run_driver(
        &self,
        operation_id: CanonicalId,
        input: Vec<u8>,
    ) -> Result<Vec<u8>, BackendRestoreCoordinatorError> {
        self.run_driver_process(operation_id, Some(input)).await
    }

    async fn inspect_driver(
        &self,
        operation_id: CanonicalId,
    ) -> Result<Option<BackendRestoreExecutionReport>, BackendRestoreCoordinatorError> {
        let output = self.run_driver_process(operation_id, None).await?;
        let report = serde_json::from_slice(&output).map_err(|_| {
            BackendRestoreCoordinatorError::Protocol(
                "topology driver status stdout was not one valid restore execution report"
                    .to_string(),
            )
        })?;
        Ok(Some(report))
    }

    async fn run_driver_process(
        &self,
        operation_id: CanonicalId,
        input: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, BackendRestoreCoordinatorError> {
        let mut command = Command::new(&self.executable);
        command.arg("--config").arg(&self.driver_config);
        if input.is_some() {
            command.arg("restore");
        } else {
            command.arg("status");
        }
        command.arg("--operation-id").arg(operation_id.to_string());
        if input.is_some() {
            command.arg("--restore-set").arg("-");
        }
        command
            .env_clear()
            .env("LANG", "C")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            BackendRestoreCoordinatorError::Unknown(format!(
                "could not start pinned topology driver: {error}"
            ))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            BackendRestoreCoordinatorError::Unknown(
                "topology driver stdout was not available".to_string(),
            )
        })?;
        let writer = if let Some(input) = input {
            let mut stdin = child.stdin.take().ok_or_else(|| {
                BackendRestoreCoordinatorError::Unknown(
                    "topology driver stdin was not available".to_string(),
                )
            })?;
            Some(tokio::spawn(async move {
                stdin.write_all(&input).await?;
                stdin.shutdown().await
            }))
        } else {
            None
        };
        let reader = tokio::spawn(async move {
            let mut output = Vec::new();
            stdout
                .take(MAX_DRIVER_OUTPUT_BYTES + 1)
                .read_to_end(&mut output)
                .await?;
            Ok::<_, std::io::Error>(output)
        });
        let status = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(result) => result.map_err(|error| {
                BackendRestoreCoordinatorError::Unknown(format!(
                    "could not wait for topology driver: {error}"
                ))
            })?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                if let Some(writer) = writer {
                    let _ = writer.await;
                }
                let _ = reader.await;
                return Err(BackendRestoreCoordinatorError::Unknown(
                    "topology driver exceeded its configured timeout".to_string(),
                ));
            }
        };
        let writer_result = if let Some(writer) = writer {
            Some(writer.await.map_err(|error| {
                BackendRestoreCoordinatorError::Unknown(format!(
                    "topology driver stdin task stopped: {error}"
                ))
            })?)
        } else {
            None
        };
        let output = reader
            .await
            .map_err(|error| {
                BackendRestoreCoordinatorError::Unknown(format!(
                    "topology driver stdout task stopped: {error}"
                ))
            })?
            .map_err(|error| {
                BackendRestoreCoordinatorError::Unknown(format!(
                    "could not read topology driver stdout: {error}"
                ))
            })?;
        if !status.success() {
            return Err(BackendRestoreCoordinatorError::Unknown(format!(
                "topology driver exited with {status}"
            )));
        }
        if let Some(writer_result) = writer_result {
            writer_result.map_err(|error| {
                BackendRestoreCoordinatorError::Unknown(format!(
                    "could not send restore input to topology driver: {error}"
                ))
            })?;
        }
        if output.len() as u64 > MAX_DRIVER_OUTPUT_BYTES {
            return Err(BackendRestoreCoordinatorError::Protocol(
                "topology driver stdout exceeded 1 MiB".to_string(),
            ));
        }
        Ok(output)
    }
}

fn paired_absolute_path(
    value: Option<&Path>,
    pair: Option<&Path>,
    executable: bool,
) -> Result<PathBuf, BackendRestoreCoordinatorError> {
    let label = if executable {
        "backend_restore_driver_executable"
    } else {
        "backend_restore_driver_config"
    };
    let value = value.ok_or_else(|| {
        BackendRestoreCoordinatorError::Configuration(format!(
            "{label} and its paired restore-driver path are required"
        ))
    })?;
    let path_is_clean = value.components().all(|component| {
        matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Normal(_)
        )
    });
    let metadata = std::fs::symlink_metadata(value).map_err(|error| {
        BackendRestoreCoordinatorError::Configuration(format!("cannot inspect {label}: {error}"))
    })?;
    if pair.is_none()
        || !value.is_absolute()
        || !path_is_clean
        || !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
    {
        return Err(BackendRestoreCoordinatorError::Configuration(format!(
            "{label} must name an existing, lexically clean absolute regular file (not a symlink), and both restore-driver paths must be configured"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 {
            return Err(BackendRestoreCoordinatorError::Configuration(format!(
                "{label} must not be writable by group or other users"
            )));
        }
        if executable && mode & 0o111 == 0 {
            return Err(BackendRestoreCoordinatorError::Configuration(
                "backend_restore_driver_executable is not executable".to_string(),
            ));
        }
    }
    Ok(value.to_path_buf())
}

fn file_sha256(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    let mut digest = Sha256::new();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use deis_wire::{
        BACKEND_RESTORE_REPORT_SCHEMA_V1, BackendRestoredComponent, RestoreBackend, RestoreDigest,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::catalog::{BackendRestoreState, OperationRequestContext};
    use crate::database::Database;

    async fn planned_restore(store: &CatalogStore, key: &str) -> BackendRestorePlan {
        let installation = store.product_installation().await.unwrap();
        let restore_set: deis_wire::BackendRestoreSet = serde_json::from_str(include_str!(
            "../../../../contracts/v1/fixtures/valid/backend-restore-set.json"
        ))
        .unwrap();
        let mut value = serde_json::to_value(restore_set).unwrap();
        value["scope"]["organizationId"] = installation.organization_id.to_string().into();
        value["scope"]["installationId"] = installation.installation_id.to_string().into();
        let restore_set = serde_json::from_value(value).unwrap();
        let context = OperationRequestContext::new(
            key,
            "POST\n/api/backend-restores\nv1",
            "account:restore-verifier".to_string(),
            CanonicalId::new_v7(),
            CanonicalId::new_v7(),
        )
        .unwrap();
        store
            .plan_backend_restore(restore_set, context)
            .await
            .unwrap()
    }

    async fn verify_terminal_case(
        state: BackendRestoreExecutionState,
        mismatched_operation: bool,
    ) -> (BackendRestoreState, deis_wire::OperationState) {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = Arc::new(CatalogStore::new(database));
        let planned = planned_restore(&store, &format!("terminal-{state:?}")).await;
        let coordinator_id = CanonicalId::new_v7();
        let (claimed, operation) = store
            .begin_backend_restore(planned.operation_id, coordinator_id, 30)
            .await
            .unwrap();
        let components = RestoreBackend::REQUIRED
            .into_iter()
            .map(|backend| BackendRestoredComponent {
                backend,
                previous_incarnation_id: CanonicalId::new_v7(),
                restored_incarnation_id: CanonicalId::new_v7(),
            })
            .collect::<Vec<_>>();
        let report = BackendRestoreExecutionReport {
            schema: BACKEND_RESTORE_REPORT_SCHEMA_V1.to_string(),
            schema_version: 1,
            operation_id: if mismatched_operation {
                CanonicalId::new_v7()
            } else {
                claimed.operation_id
            },
            restore_set_id: claimed.restore_set_id,
            restore_set_sha256: RestoreDigest {
                algorithm: "sha256".to_string(),
                value: claimed.restore_set_digest.value.clone(),
            },
            state,
            components,
        };

        let coordinator = BackendRestoreCoordinator {
            catalog: Arc::clone(&store),
            executable: PathBuf::from("/bin/true"),
            driver_config: directory.path().join("unused.toml"),
            timeout: Duration::from_secs(1),
            coordinator_id,
        };
        let outcome = coordinator
            .accept_terminal_report(claimed.operation_id, report, operation.revision)
            .await;
        if mismatched_operation || state == BackendRestoreExecutionState::RolledBack {
            assert!(outcome.is_err());
        } else {
            outcome.unwrap();
        }
        let persisted = store
            .backend_restore(planned.restore_id)
            .await
            .unwrap()
            .unwrap();
        let operation = store
            .product_operation(planned.operation_id)
            .await
            .unwrap()
            .unwrap();
        (persisted.state, operation.state)
    }

    #[tokio::test]
    async fn only_a_report_bound_to_the_claimed_restore_is_committed() {
        let succeeded = verify_terminal_case(BackendRestoreExecutionState::Succeeded, false).await;
        assert_eq!(succeeded.0, BackendRestoreState::Succeeded);
        assert_eq!(succeeded.1, deis_wire::OperationState::Succeeded);

        let rolled_back =
            verify_terminal_case(BackendRestoreExecutionState::RolledBack, false).await;
        assert_eq!(rolled_back.0, BackendRestoreState::Failed);
        assert_eq!(rolled_back.1, deis_wire::OperationState::Failed);

        let wrong_operation =
            verify_terminal_case(BackendRestoreExecutionState::Succeeded, true).await;
        assert_eq!(wrong_operation.0, BackendRestoreState::Waiting);
        assert_eq!(wrong_operation.1, deis_wire::OperationState::Waiting);
    }
}
