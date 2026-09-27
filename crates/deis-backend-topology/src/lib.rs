//! Embedded topology driver for coordinated store/queue/telemetry restore.
//!
//! The driver stages all three verified snapshots before stopping anything,
//! then replaces their data directories under one recorded outage. Every
//! directory move is journaled so the same operation can finish after a
//! process crash. If switching or verification fails, the old directories
//! are restored before services are restarted.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::Utc;
use deis_backend_migration::TenantWriteFences;
use deis_wire::{
    BACKEND_RESTORE_REPORT_SCHEMA_V1, BACKEND_RESTORE_SET_SCHEMA_V1, BackendRestoreExecutionReport,
    BackendRestoreExecutionState, BackendRestoreSet, BackendRestoredComponent, CanonicalId,
    RestoreBackend, RestoreBarrierKind, RestoreComponent, RestoreConsistencyBarrier, RestoreDigest,
    RestoreScope, RestoreScopeKind, RestoreTopologyDriver, durability::sync_parent_directory,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::process::Command;

const JOURNAL_VERSION: u32 = 1;
const SNAPSHOT_CAPTURE_JOURNAL_VERSION: u32 = 1;
const SNAPSHOT_CAPTURE_JOURNAL: &str = "snapshot-capture.json";
const FENCE_JOURNAL: &str = "migration-fences.sqlite";
const MANIFEST_NAME: &str = "snapshot.json";

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyConfig {
    pub schema_version: u32,
    pub driver_id: String,
    pub driver_version: String,
    pub distribution_sha256: String,
    pub state_dir: PathBuf,
    /// Local roots under which snapshot sets may be created or consumed.
    /// A restore administrator chooses an artifact, not an arbitrary file the
    /// more privileged topology-driver account can read.
    pub snapshot_roots: Vec<PathBuf>,
    pub writer_admission: WriterAdmissionConfig,
    pub store: ServiceConfig,
    pub queue: ServiceConfig,
    pub telemetry: ServiceConfig,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriterAdmissionConfig {
    /// Idempotently prevent application/runtime writes from reaching any
    /// embedded backend before the first backend is stopped.
    pub close: ServiceCommand,
    /// Idempotently reopen writes only after all backends are reachable in a
    /// single verified topology.
    pub open: ServiceCommand,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    pub stop: ServiceCommand,
    pub start: ServiceCommand,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceCommand {
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_command_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_command_timeout_seconds() -> u64 {
    30
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum RestorePhase {
    Planning,
    Staging,
    Staged,
    ClosingAdmission,
    AdmissionClosed,
    Stopping,
    Stopped,
    Switched,
    Started,
    OpeningAdmission,
    Verified,
    RollingBack,
    RolledBack,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum SnapshotCapturePhase {
    Planning,
    AdmissionClosed,
    Stopped,
    Captured,
    Completed,
    Aborted,
}

impl SnapshotCapturePhase {
    const fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Aborted)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotCaptureJournal {
    schema_version: u32,
    capture_id: CanonicalId,
    output_dir: PathBuf,
    organization_id: CanonicalId,
    installation_id: CanonicalId,
    phase: SnapshotCapturePhase,
    restore_set: Option<BackendRestoreSet>,
    last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreJournal {
    schema_version: u32,
    operation_id: CanonicalId,
    restore_set_id: CanonicalId,
    restore_set_sha256: String,
    phase: RestorePhase,
    components: Vec<ComponentJournal>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ComponentJournal {
    backend: RestoreBackend,
    live_dir: PathBuf,
    stage_dir: PathBuf,
    rollback_dir: PathBuf,
    failed_dir: PathBuf,
    previous_incarnation_id: Option<CanonicalId>,
    restored_incarnation_id: Option<CanonicalId>,
    staged_tree_sha256: Option<String>,
    switched: bool,
}

impl TopologyConfig {
    pub fn load(path: &Path) -> Result<Self> {
        validate_trusted_regular_file(path, "topology config")?;
        let text = fs::read_to_string(path)
            .with_context(|| format!("cannot read topology config {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("cannot parse topology config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "topology config schema_version must be 1"
        );
        ensure!(
            valid_driver_id(&self.driver_id),
            "topology driver_id is not canonical"
        );
        ensure!(
            valid_semver(&self.driver_version),
            "topology driver_version is not pinned semantic version text"
        );
        validate_sha256(&self.distribution_sha256, "topology distribution")?;
        ensure!(
            self.state_dir.is_absolute() && path_is_lexically_clean(&self.state_dir),
            "topology state_dir must be a lexically clean absolute path"
        );
        ensure!(
            !self.snapshot_roots.is_empty(),
            "topology snapshot_roots must contain at least one local backup root"
        );
        validate_unscoped_command(&self.writer_admission.close, "writer admission close")?;
        validate_unscoped_command(&self.writer_admission.open, "writer admission open")?;

        let services = self.services();
        let mut data_dirs = BTreeSet::new();
        let mut listeners = BTreeSet::new();
        for (backend, service) in services {
            ensure!(
                service.data_dir.is_absolute() && path_is_lexically_clean(&service.data_dir),
                "{} data_dir must be a lexically clean absolute path",
                backend.as_str()
            );
            reject_existing_ancestor_symlinks(
                &service.data_dir,
                &format!("{} data directory ancestor", backend.as_str()),
            )?;
            ensure!(
                service.listen.ip().is_loopback(),
                "{} embedded listener must be loopback-only so writer admission is authoritative",
                backend.as_str()
            );
            ensure!(
                data_dirs.insert(service.data_dir.clone()),
                "backend data_dir values must be distinct"
            );
            ensure!(
                listeners.insert(service.listen),
                "backend listen addresses must be distinct"
            );
            validate_command(&service.stop, "stop", backend)?;
            validate_command(&service.start, "start", backend)?;
        }
        for left in &data_dirs {
            ensure!(
                !left.starts_with(&self.state_dir) && !self.state_dir.starts_with(left),
                "topology state_dir and backend data_dir must not contain one another"
            );
            for right in &data_dirs {
                if left != right {
                    ensure!(
                        !left.starts_with(right) && !right.starts_with(left),
                        "backend data_dir values must not contain one another"
                    );
                }
            }
        }
        let mut snapshot_roots = BTreeSet::new();
        for root in &self.snapshot_roots {
            ensure!(
                root.is_absolute() && path_is_lexically_clean(root),
                "snapshot root must be a lexically clean absolute path"
            );
            reject_existing_ancestor_symlinks(root, "snapshot root ancestor")?;
            ensure!(
                snapshot_roots.insert(root),
                "snapshot_roots must not contain duplicates"
            );
            ensure!(
                !root.starts_with(&self.state_dir) && !self.state_dir.starts_with(root),
                "snapshot root and topology state_dir must not contain one another"
            );
            for data_dir in &data_dirs {
                ensure!(
                    !root.starts_with(data_dir) && !data_dir.starts_with(root),
                    "snapshot root and backend data_dir must not contain one another"
                );
            }
        }
        Ok(())
    }

    fn services(&self) -> [(RestoreBackend, &ServiceConfig); 3] {
        [
            (RestoreBackend::DeisStore, &self.store),
            (RestoreBackend::DeisQueue, &self.queue),
            (RestoreBackend::DeisTelemetry, &self.telemetry),
        ]
    }
}

/// Stop all embedded backend writers and services, capture a single outage
/// recovery point, then reopen the platform before publishing the set.
pub async fn capture_restore_set(
    config: &TopologyConfig,
    out: &Path,
    organization_id: CanonicalId,
    installation_id: CanonicalId,
) -> Result<BackendRestoreSet> {
    capture_restore_set_inner(config, out, organization_id, installation_id, true, true).await
}

/// Recover an interrupted snapshot capture without beginning a restore or a
/// second capture. If all three snapshots were durably recorded, recovery
/// reopens the platform and publishes that exact restore set; otherwise it
/// reopens the platform, retains partial output for diagnosis, and marks the
/// capture aborted.
pub async fn recover_snapshot_capture(
    config: &TopologyConfig,
) -> Result<Option<BackendRestoreSet>> {
    config.validate()?;
    let executable = std::env::current_exe().context("cannot locate topology driver executable")?;
    ensure!(
        file_sha256(&executable)? == config.distribution_sha256,
        "running topology driver distribution digest differs from its config"
    );
    ensure_secure_state_dir(&config.state_dir)?;
    let _topology_lock = acquire_lock(
        &config.state_dir.join("topology.lock"),
        "another topology operation is changing embedded backend state",
    )?;
    recover_interrupted_snapshot_capture_locked(
        config,
        &config.state_dir.join(SNAPSHOT_CAPTURE_JOURNAL),
        true,
    )
    .await
}

async fn capture_restore_set_inner(
    config: &TopologyConfig,
    out: &Path,
    organization_id: CanonicalId,
    installation_id: CanonicalId,
    probe_after_start: bool,
    verify_distribution: bool,
) -> Result<BackendRestoreSet> {
    config.validate()?;
    if verify_distribution {
        let executable =
            std::env::current_exe().context("cannot locate topology driver executable")?;
        ensure!(
            file_sha256(&executable)? == config.distribution_sha256,
            "running topology driver distribution digest differs from its config"
        );
    }
    ensure_secure_state_dir(&config.state_dir)?;
    let _topology_lock = acquire_lock(
        &config.state_dir.join("topology.lock"),
        "another topology operation is changing embedded backend state",
    )?;
    let capture_journal_path = config.state_dir.join(SNAPSHOT_CAPTURE_JOURNAL);
    recover_interrupted_snapshot_capture_locked(config, &capture_journal_path, probe_after_start)
        .await?;
    if path_entry_exists(&capture_journal_path)? {
        let existing = load_snapshot_capture_journal(config, &capture_journal_path)?;
        if existing.phase == SnapshotCapturePhase::Completed
            && existing.output_dir == out
            && existing.organization_id == organization_id
            && existing.installation_id == installation_id
        {
            let restore_set = existing
                .restore_set
                .context("completed snapshot capture has no restore set")?;
            publish_or_verify_restore_set(out, &restore_set)?;
            return Ok(restore_set);
        }
    }

    validate_snapshot_root(config, out)?;
    prepare_empty_directory(out, "snapshot-set output")?;
    let mut journal = SnapshotCaptureJournal {
        schema_version: SNAPSHOT_CAPTURE_JOURNAL_VERSION,
        capture_id: CanonicalId::new_v7(),
        output_dir: out.to_path_buf(),
        organization_id,
        installation_id,
        phase: SnapshotCapturePhase::Planning,
        restore_set: None,
        last_error: None,
    };
    persist_snapshot_capture_journal(&capture_journal_path, &journal)?;

    let capture = async {
        run_command(&config.writer_admission.close)
            .await
            .context("could not close backend writer admission")?;
        let writer_admission_closed_at = Utc::now();
        journal.phase = SnapshotCapturePhase::AdmissionClosed;
        persist_snapshot_capture_journal(&capture_journal_path, &journal)?;
        for (_, service) in config.services().into_iter().rev() {
            run_command(&service.stop).await?;
        }
        for (backend, service) in config.services() {
            wait_unreachable(service.listen).await.with_context(|| {
                format!(
                    "{} remained reachable after its stop command",
                    backend.as_str()
                )
            })?;
        }
        let all_services_stopped_at = Utc::now();
        let source_ids = config
            .services()
            .into_iter()
            .map(|(backend, service)| {
                current_incarnation(&service.data_dir)?.ok_or_else(|| {
                    anyhow!(
                        "{} has no durable current incarnation to bind into a restore set",
                        backend.as_str()
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        journal.phase = SnapshotCapturePhase::Stopped;
        persist_snapshot_capture_journal(&capture_journal_path, &journal)?;
        let snapshot_dirs = [
            out.join(RestoreBackend::DeisStore.as_str()),
            out.join(RestoreBackend::DeisQueue.as_str()),
            out.join(RestoreBackend::DeisTelemetry.as_str()),
        ];
        let store_manifest = deis_store::snapshot::take(&config.store.data_dir, &snapshot_dirs[0])?;
        ensure!(
            store_manifest.source_backend_incarnation_id == Some(source_ids[0]),
            "store changed identity while its stopped snapshot was captured"
        );
        let store_captured_at = Utc::now();
        let queue_manifest = deis_queue::snapshot::take(&config.queue.data_dir, &snapshot_dirs[1])?;
        ensure!(
            queue_manifest.source_backend_incarnation_id == source_ids[1],
            "queue changed identity while its stopped snapshot was captured"
        );
        let queue_captured_at = Utc::now();
        let telemetry_manifest =
            deis_telemetry::snapshot::take(&config.telemetry.data_dir, &snapshot_dirs[2])?;
        ensure!(
            telemetry_manifest.source_backend_incarnation_id == source_ids[2],
            "telemetry changed identity while its stopped snapshot was captured"
        );
        let telemetry_captured_at = Utc::now();
        let snapshot_completed_at = Utc::now();
        let captured_at = [store_captured_at, queue_captured_at, telemetry_captured_at];
        let components = RestoreBackend::REQUIRED
            .into_iter()
            .enumerate()
            .map(|(index, backend)| {
                Ok(RestoreComponent {
                    backend,
                    snapshot_artifact_id: CanonicalId::new_v7(),
                    snapshot_uri: format!("file://{}", snapshot_dirs[index].display()),
                    snapshot_manifest_digest: RestoreDigest {
                        algorithm: "sha256".to_string(),
                        value: file_sha256(&snapshot_dirs[index].join(MANIFEST_NAME))?,
                    },
                    source_backend_incarnation_id: source_ids[index],
                    size_bytes: tree_bytes(&snapshot_dirs[index])?,
                    captured_at: captured_at[index],
                    contains_fence_journal: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let restore_set = BackendRestoreSet {
            schema: BACKEND_RESTORE_SET_SCHEMA_V1.to_string(),
            schema_version: 1,
            restore_set_id: CanonicalId::new_v7(),
            scope: RestoreScope {
                kind: RestoreScopeKind::Installation,
                organization_id,
                installation_id,
            },
            consistency_barrier: RestoreConsistencyBarrier {
                kind: RestoreBarrierKind::FullPlatformOutage,
                barrier_id: CanonicalId::new_v7(),
                writer_admission_closed_at,
                all_services_stopped_at,
                snapshot_completed_at,
            },
            topology_driver: RestoreTopologyDriver {
                id: config.driver_id.clone(),
                version: config.driver_version.clone(),
                distribution_digest: RestoreDigest {
                    algorithm: "sha256".to_string(),
                    value: config.distribution_sha256.clone(),
                },
            },
            components,
            created_at: Utc::now(),
        };
        restore_set.validate()?;
        journal.phase = SnapshotCapturePhase::Captured;
        journal.restore_set = Some(restore_set.clone());
        persist_snapshot_capture_journal(&capture_journal_path, &journal)?;
        Ok::<_, anyhow::Error>(restore_set)
    }
    .await;

    match capture {
        Ok(_) => recover_interrupted_snapshot_capture_locked(
            config,
            &capture_journal_path,
            probe_after_start,
        )
        .await?
        .context("captured snapshot journal did not produce a restore set"),
        Err(capture_error) => {
            match recover_interrupted_snapshot_capture_locked(
                config,
                &capture_journal_path,
                probe_after_start,
            )
            .await
            {
                Ok(_) => Err(capture_error.context(
                    "snapshot set was not published; services and writer admission were restored",
                )),
                Err(recovery_error) => Err(capture_error.context(format!(
                    "snapshot capture failed and platform recovery also failed: {recovery_error:#}"
                ))),
            }
        }
    }
}

async fn recover_interrupted_snapshot_capture_locked(
    config: &TopologyConfig,
    journal_path: &Path,
    probe_after_start: bool,
) -> Result<Option<BackendRestoreSet>> {
    if !path_entry_exists(journal_path)? {
        return Ok(None);
    }
    let mut journal = load_snapshot_capture_journal(config, journal_path)?;
    match journal.phase {
        SnapshotCapturePhase::Completed => return Ok(journal.restore_set),
        SnapshotCapturePhase::Aborted => return Ok(None),
        _ => {}
    }

    restart_services_and_open_admission(config, probe_after_start)
        .await
        .context("could not recover services and writer admission after snapshot capture")?;
    if let Some(restore_set) = journal.restore_set.clone() {
        if let Err(error) = verify_snapshot_artifact_envelopes(config, &restore_set) {
            journal.phase = SnapshotCapturePhase::Aborted;
            journal.restore_set = None;
            journal.last_error = Some(
                "captured snapshot artifacts failed envelope verification after platform recovery; output was retained"
                    .to_string(),
            );
            persist_snapshot_capture_journal(journal_path, &journal)?;
            return Err(error.context(
                "captured snapshot artifacts changed before the restore set could be published",
            ));
        }
        publish_or_verify_restore_set(&journal.output_dir, &restore_set)?;
        journal.phase = SnapshotCapturePhase::Completed;
        journal.last_error = None;
        persist_snapshot_capture_journal(journal_path, &journal)?;
        Ok(Some(restore_set))
    } else {
        journal.phase = SnapshotCapturePhase::Aborted;
        journal.last_error = Some(
            "snapshot capture ended before a complete restore set was durably recorded; partial output was retained"
                .to_string(),
        );
        persist_snapshot_capture_journal(journal_path, &journal)?;
        Ok(None)
    }
}

fn load_snapshot_capture_journal(
    config: &TopologyConfig,
    path: &Path,
) -> Result<SnapshotCaptureJournal> {
    reject_symlink(path, "snapshot capture journal")?;
    let journal: SnapshotCaptureJournal = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        journal.schema_version == SNAPSHOT_CAPTURE_JOURNAL_VERSION,
        "unknown snapshot capture journal schema"
    );
    validate_snapshot_root(config, &journal.output_dir)
        .context("snapshot capture journal output path is no longer trusted")?;
    ensure!(
        journal.phase.terminal() || journal.last_error.is_none(),
        "non-terminal snapshot capture journal must not claim a terminal error"
    );
    ensure!(
        (journal.phase == SnapshotCapturePhase::Aborted) == journal.last_error.is_some(),
        "snapshot capture journal phase and terminal error disagree"
    );
    if let Some(error) = &journal.last_error {
        ensure!(
            !error.is_empty() && error.len() <= 2048 && !error.chars().any(char::is_control),
            "snapshot capture journal error is not a bounded printable diagnostic"
        );
    }
    ensure!(
        matches!(
            journal.phase,
            SnapshotCapturePhase::Captured | SnapshotCapturePhase::Completed
        ) == journal.restore_set.is_some(),
        "snapshot capture journal phase and restore set disagree"
    );
    if let Some(restore_set) = &journal.restore_set {
        restore_set.validate()?;
        ensure!(
            restore_set.scope.organization_id == journal.organization_id
                && restore_set.scope.installation_id == journal.installation_id,
            "snapshot capture journal scope and restore set disagree"
        );
        ensure!(
            restore_set.topology_driver.id == config.driver_id
                && restore_set.topology_driver.version == config.driver_version
                && restore_set.topology_driver.distribution_digest.value
                    == config.distribution_sha256,
            "snapshot capture journal is pinned to another topology driver build"
        );
        for component in &restore_set.components {
            ensure!(
                component.snapshot_uri
                    == format!(
                        "file://{}",
                        journal
                            .output_dir
                            .join(component.backend.as_str())
                            .display()
                    ),
                "snapshot capture journal restore set points outside its output directory"
            );
        }
    }
    Ok(journal)
}

fn persist_snapshot_capture_journal(path: &Path, journal: &SnapshotCaptureJournal) -> Result<()> {
    let parent = path
        .parent()
        .context("snapshot capture journal has no parent")?;
    reject_symlink(parent, "snapshot capture journal directory")?;
    let temporary = parent.join(".snapshot-capture.tmp");
    prepare_atomic_temporary(&temporary, "snapshot capture journal")?;
    let bytes = serde_json::to_vec_pretty(journal)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    sync_parent_directory(path)?;
    Ok(())
}

fn publish_or_verify_restore_set(out: &Path, restore_set: &BackendRestoreSet) -> Result<()> {
    let manifest_path = out.join("restore-set.json");
    match fs::symlink_metadata(&manifest_path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "published restore set must be a regular file"
            );
            let published: BackendRestoreSet = serde_json::from_slice(&fs::read(&manifest_path)?)?;
            ensure!(
                published == *restore_set,
                "published restore set differs from its capture journal"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            persist_restore_set(&manifest_path, restore_set)
        }
        Err(error) => Err(error.into()),
    }
}

fn verify_snapshot_artifact_envelopes(
    config: &TopologyConfig,
    restore_set: &BackendRestoreSet,
) -> Result<()> {
    for component in &restore_set.components {
        let snapshot = local_snapshot_path(config, &component.snapshot_uri)?;
        reject_existing_ancestor_symlinks(&snapshot, "snapshot artifact path")?;
        reject_tree_symlinks(&snapshot)?;
        let manifest = snapshot.join(MANIFEST_NAME);
        ensure!(
            manifest.is_file()
                && file_sha256(&manifest)? == component.snapshot_manifest_digest.value,
            "{} snapshot manifest changed after capture",
            component.backend.as_str()
        );
        ensure!(
            tree_bytes(&snapshot)? == component.size_bytes,
            "{} snapshot byte count changed after capture",
            component.backend.as_str()
        );
    }
    Ok(())
}

/// Execute or resume one restore. Reusing the operation ID with different
/// input is refused before any command or filesystem mutation.
pub async fn execute_restore(
    config: &TopologyConfig,
    restore_set: &BackendRestoreSet,
    operation_id: CanonicalId,
) -> Result<BackendRestoreExecutionReport> {
    execute_restore_inner(config, restore_set, operation_id, true, false, true).await
}

/// Inspect the durable journal after an interrupted driver invocation.
///
/// Only a physically verified terminal state is returned as evidence. A
/// non-terminal journal produces `None`, so Control cannot mistake a process
/// exit, lock race, or partially persisted directory switch for completion.
pub fn inspect_restore(
    config: &TopologyConfig,
    operation_id: CanonicalId,
) -> Result<Option<BackendRestoreExecutionReport>> {
    inspect_restore_inner(config, operation_id, true)
}

fn inspect_restore_inner(
    config: &TopologyConfig,
    operation_id: CanonicalId,
    verify_distribution: bool,
) -> Result<Option<BackendRestoreExecutionReport>> {
    config.validate()?;
    if verify_distribution {
        let executable =
            std::env::current_exe().context("cannot locate topology driver executable")?;
        ensure!(
            file_sha256(&executable)? == config.distribution_sha256,
            "running topology driver distribution digest differs from its config"
        );
    }
    ensure!(
        config.state_dir.is_dir(),
        "topology state directory does not exist"
    );
    ensure_secure_state_dir(&config.state_dir)?;
    let _topology_lock = acquire_lock(
        &config.state_dir.join("topology.lock"),
        "another topology operation is changing embedded backend state",
    )?;
    let capture_journal_path = config.state_dir.join(SNAPSHOT_CAPTURE_JOURNAL);
    if path_entry_exists(&capture_journal_path)? {
        let capture = load_snapshot_capture_journal(config, &capture_journal_path)?;
        ensure!(
            capture.phase.terminal(),
            "snapshot capture {} is {:?}; a mutating topology command must recover it before restore status can be trusted",
            capture.capture_id,
            capture.phase
        );
    }
    let lock_path = config
        .state_dir
        .join(format!("restore-{operation_id}.lock"));
    let _lock = acquire_lock(&lock_path, "topology restore is still executing")?;
    let journal_path = config
        .state_dir
        .join(format!("restore-{operation_id}.json"));
    ensure!(journal_path.is_file(), "restore journal does not exist");
    let journal = load_journal(&journal_path)?;
    ensure!(
        journal.operation_id == operation_id,
        "restore journal operation identity changed"
    );
    validate_journal_bindings(config, &journal)?;
    match journal.phase {
        RestorePhase::Verified => {
            for component in &journal.components {
                verify_switched(component)?;
            }
            Ok(Some(report(
                &journal,
                BackendRestoreExecutionState::Succeeded,
            )?))
        }
        RestorePhase::RolledBack => {
            for component in &journal.components {
                ensure!(
                    !component.switched
                        && current_incarnation(&component.live_dir)?
                            == component.previous_incarnation_id,
                    "{} journal says rolled back but the prior incarnation is not live",
                    component.backend.as_str()
                );
            }
            Ok(Some(report(
                &journal,
                BackendRestoreExecutionState::RolledBack,
            )?))
        }
        _ => Ok(None),
    }
}

async fn execute_restore_inner(
    config: &TopologyConfig,
    restore_set: &BackendRestoreSet,
    operation_id: CanonicalId,
    probe_after_start: bool,
    fail_after_switch: bool,
    verify_distribution: bool,
) -> Result<BackendRestoreExecutionReport> {
    config.validate()?;
    if verify_distribution {
        let executable =
            std::env::current_exe().context("cannot locate topology driver executable")?;
        ensure!(
            file_sha256(&executable)? == config.distribution_sha256,
            "running topology driver distribution digest differs from its config"
        );
    }
    restore_set
        .validate()
        .context("restore set failed shared-contract validation")?;
    ensure!(
        restore_set.topology_driver.id == config.driver_id
            && restore_set.topology_driver.version == config.driver_version
            && restore_set.topology_driver.distribution_digest.value == config.distribution_sha256,
        "restore set is pinned to another topology driver build"
    );
    let restore_set_sha256 = sha256_hex(&restore_set.canonical_json()?);
    ensure_secure_state_dir(&config.state_dir)?;
    let _topology_lock = acquire_lock(
        &config.state_dir.join("topology.lock"),
        "another topology operation is changing embedded backend state",
    )?;
    recover_interrupted_snapshot_capture_locked(
        config,
        &config.state_dir.join(SNAPSHOT_CAPTURE_JOURNAL),
        probe_after_start,
    )
    .await?;
    let lock_path = config
        .state_dir
        .join(format!("restore-{operation_id}.lock"));
    let _operation_lock = acquire_lock(
        &lock_path,
        "another topology driver process is executing this restore operation",
    )?;
    let journal_path = config
        .state_dir
        .join(format!("restore-{operation_id}.json"));
    let mut journal = if journal_path.exists() {
        let journal = load_journal(&journal_path)?;
        ensure!(
            journal.operation_id == operation_id,
            "restore journal operation identity changed"
        );
        validate_journal_bindings(config, &journal)?;
        ensure!(
            journal.restore_set_id == restore_set.restore_set_id
                && journal.restore_set_sha256 == restore_set_sha256,
            "operation ID already belongs to another restore set"
        );
        journal
    } else {
        let mut components = Vec::with_capacity(3);
        for (backend, service) in config.services() {
            let paths = component_paths(service, operation_id, backend)?;
            components.push(ComponentJournal {
                backend,
                live_dir: service.data_dir.clone(),
                stage_dir: paths.0,
                rollback_dir: paths.1,
                failed_dir: paths.2,
                previous_incarnation_id: Some(
                    current_incarnation(&service.data_dir)?.ok_or_else(|| {
                        anyhow!(
                            "{} has no durable current incarnation; this driver cannot promise rollback",
                            backend.as_str()
                        )
                    })?,
                ),
                restored_incarnation_id: None,
                staged_tree_sha256: None,
                switched: false,
            });
        }
        let journal = RestoreJournal {
            schema_version: JOURNAL_VERSION,
            operation_id,
            restore_set_id: restore_set.restore_set_id,
            restore_set_sha256: restore_set_sha256.clone(),
            phase: RestorePhase::Planning,
            components,
        };
        persist_journal(&journal_path, &journal)?;
        journal
    };

    if journal.phase == RestorePhase::Verified {
        return report(&journal, BackendRestoreExecutionState::Succeeded);
    }
    if journal.phase == RestorePhase::RolledBack {
        bail!("restore operation was rolled back and cannot be resumed; create a new operation");
    }
    if journal.phase == RestorePhase::RollingBack {
        rollback(config, &journal_path, &mut journal, probe_after_start).await?;
        bail!("restore operation was rolled back after resuming an interrupted rollback");
    }

    let result = async {
        if matches!(
            journal.phase,
            RestorePhase::Planning | RestorePhase::Staging | RestorePhase::Staged
        ) {
            journal.phase = RestorePhase::Staging;
            persist_journal(&journal_path, &journal)?;
            for index in 0..journal.components.len() {
                if journal.components[index].restored_incarnation_id.is_some() {
                    if verify_staged(&journal.components[index]).is_ok() {
                        continue;
                    }
                    reset_temporary_directory(
                        &journal.components[index].stage_dir,
                        operation_id,
                        ".stage",
                    )?;
                    journal.components[index].restored_incarnation_id = None;
                    journal.components[index].staged_tree_sha256 = None;
                    persist_journal(&journal_path, &journal)?;
                }
                let input = &restore_set.components[index];
                ensure!(
                    input.backend == journal.components[index].backend,
                    "restore set/journal component order changed"
                );
                reset_temporary_directory(
                    &journal.components[index].stage_dir,
                    operation_id,
                    ".stage",
                )?;
                let restored =
                    stage_component(config, input, &journal.components[index].stage_dir)?;
                ensure!(
                    restored != input.source_backend_incarnation_id,
                    "{} restore reused its source backend incarnation",
                    input.backend.as_str()
                );
                journal.components[index].restored_incarnation_id = Some(restored);
                journal.components[index].staged_tree_sha256 =
                    Some(tree_sha256(&journal.components[index].stage_dir)?);
                persist_journal(&journal_path, &journal)?;
            }
            journal.phase = RestorePhase::Staged;
            persist_journal(&journal_path, &journal)?;
        }

        if journal.phase == RestorePhase::Staged {
            journal.phase = RestorePhase::ClosingAdmission;
            persist_journal(&journal_path, &journal)?;
        }

        if journal.phase == RestorePhase::ClosingAdmission {
            run_command(&config.writer_admission.close).await?;
            journal.phase = RestorePhase::AdmissionClosed;
            persist_journal(&journal_path, &journal)?;
        }

        if journal.phase == RestorePhase::AdmissionClosed {
            journal.phase = RestorePhase::Stopping;
            persist_journal(&journal_path, &journal)?;
        }

        if journal.phase == RestorePhase::Stopping {
            for (_, service) in config.services().into_iter().rev() {
                run_command(&service.stop).await?;
            }
            for (backend, service) in config.services() {
                wait_unreachable(service.listen).await.with_context(|| {
                    format!(
                        "{} remained reachable after its stop command",
                        backend.as_str()
                    )
                })?;
            }
            for component in &journal.components {
                ensure!(
                    current_incarnation(&component.live_dir)? == component.previous_incarnation_id,
                    "{} data incarnation changed between restore planning and the stop barrier",
                    component.backend.as_str()
                );
                verify_staged(component).with_context(|| {
                    format!(
                        "{} staged data changed before the directory switch",
                        component.backend.as_str()
                    )
                })?;
            }
            journal.phase = RestorePhase::Stopped;
            persist_journal(&journal_path, &journal)?;
        }

        if journal.phase == RestorePhase::Stopped {
            for index in 0..journal.components.len() {
                switch_component(&mut journal.components[index])?;
                persist_journal(&journal_path, &journal)?;
            }
            journal.phase = RestorePhase::Switched;
            persist_journal(&journal_path, &journal)?;
        }

        if journal.phase == RestorePhase::Switched {
            for (_, service) in config.services() {
                run_command(&service.start).await?;
            }
            journal.phase = RestorePhase::Started;
            persist_journal(&journal_path, &journal)?;
            if fail_after_switch {
                bail!("injected post-switch failure");
            }
        }

        if journal.phase == RestorePhase::Started {
            if probe_after_start {
                for (backend, service) in config.services() {
                    wait_reachable(service.listen).await.with_context(|| {
                        format!(
                            "{} did not become reachable after restore",
                            backend.as_str()
                        )
                    })?;
                }
            }
            for component in &journal.components {
                ensure!(
                    current_incarnation(&component.live_dir)? == component.restored_incarnation_id,
                    "{} started with an incarnation other than the staged restore",
                    component.backend.as_str()
                );
            }
            journal.phase = RestorePhase::OpeningAdmission;
            persist_journal(&journal_path, &journal)?;
        }
        if journal.phase == RestorePhase::OpeningAdmission {
            run_command(&config.writer_admission.open).await?;
            journal.phase = RestorePhase::Verified;
            persist_journal(&journal_path, &journal)?;
        }
        report(&journal, BackendRestoreExecutionState::Succeeded)
    }
    .await;

    match result {
        Ok(report) => Ok(report),
        Err(error) if journal.phase == RestorePhase::OpeningAdmission => Err(error.context(
            "restored backends are verified but writer admission could not be reopened; retry the same operation",
        )),
        Err(error)
            if journal
                .components
                .iter()
                .any(|component| component.switched) =>
        {
            rollback(
                config,
                &journal_path,
                &mut journal,
                probe_after_start,
            )
            .await?;
            Err(error.context(
                "restore failed after switching data; the prior directories were restored",
            ))
        }
        Err(error)
            if matches!(
                journal.phase,
                RestorePhase::ClosingAdmission
                    | RestorePhase::AdmissionClosed
                    | RestorePhase::Stopping
                    | RestorePhase::Stopped
            ) && !journal
                .components
                .iter()
                .any(|component| component.switched) =>
        {
            let recovery = restart_services_and_open_admission(config, probe_after_start).await;
            journal.phase = RestorePhase::Staged;
            persist_journal(&journal_path, &journal)?;
            match recovery {
                Ok(()) => Err(error.context(
                    "restore stopped services but switched no data; services and writer admission were restored",
                )),
                Err(recovery_error) => Err(error.context(format!(
                    "restore switched no data, but platform recovery also failed: {recovery_error:#}; retry starts again from the staging barrier"
                ))),
            }
        }
        Err(error) => Err(error),
    }
}

fn component_paths(
    service: &ServiceConfig,
    operation_id: CanonicalId,
    backend: RestoreBackend,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let parent = service
        .data_dir
        .parent()
        .ok_or_else(|| anyhow!("{} data_dir has no parent", backend.as_str()))?;
    let name = service
        .data_dir
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| anyhow!("{} data_dir has no UTF-8 final component", backend.as_str()))?;
    let stem = format!(".{name}.restore-{operation_id}-{}", backend.as_str());
    Ok((
        parent.join(format!("{stem}.stage")),
        parent.join(format!("{stem}.rollback")),
        parent.join(format!("{stem}.failed")),
    ))
}

fn validate_journal_bindings(config: &TopologyConfig, journal: &RestoreJournal) -> Result<()> {
    ensure!(
        journal.components.len() == RestoreBackend::REQUIRED.len(),
        "restore journal component set is incomplete"
    );
    for (index, (backend, service)) in config.services().into_iter().enumerate() {
        let component = &journal.components[index];
        let (stage_dir, rollback_dir, failed_dir) =
            component_paths(service, journal.operation_id, backend)?;
        ensure!(
            component.backend == backend
                && component.live_dir == service.data_dir
                && component.stage_dir == stage_dir
                && component.rollback_dir == rollback_dir
                && component.failed_dir == failed_dir,
            "{} restore journal paths do not match the trusted topology config and operation ID",
            backend.as_str()
        );
    }
    Ok(())
}

fn stage_component(
    config: &TopologyConfig,
    component: &RestoreComponent,
    stage_dir: &Path,
) -> Result<CanonicalId> {
    let snapshot = local_snapshot_path(config, &component.snapshot_uri)?;
    reject_existing_ancestor_symlinks(&snapshot, "snapshot path")?;
    reject_tree_symlinks(&snapshot)?;
    let manifest = snapshot.join(MANIFEST_NAME);
    ensure!(
        manifest.is_file(),
        "snapshot {} has no {MANIFEST_NAME}",
        snapshot.display()
    );
    ensure!(
        file_sha256(&manifest)? == component.snapshot_manifest_digest.value,
        "{} snapshot manifest digest differs from the restore set",
        component.backend.as_str()
    );
    ensure!(
        tree_bytes(&snapshot)? == component.size_bytes,
        "{} snapshot byte count differs from the restore set",
        component.backend.as_str()
    );
    let (source, restored) = match component.backend {
        RestoreBackend::DeisStore => {
            let report = deis_store::snapshot::restore(&snapshot, stage_dir)?;
            let source = report.source_backend_incarnation_id.ok_or_else(|| {
                anyhow!("store snapshot is legacy and cannot prove its source incarnation")
            })?;
            (source, report.restored_backend_incarnation_id)
        }
        RestoreBackend::DeisQueue => {
            let report = deis_queue::snapshot::restore(&snapshot, stage_dir)?;
            (
                report.source_backend_incarnation_id,
                report.restored_backend_incarnation_id,
            )
        }
        RestoreBackend::DeisTelemetry => {
            let report = deis_telemetry::snapshot::restore(&snapshot, stage_dir)?;
            (
                report.source_backend_incarnation_id,
                report.restored_backend_incarnation_id,
            )
        }
    };
    ensure!(
        source == component.source_backend_incarnation_id,
        "{} snapshot source incarnation differs from the restore set",
        component.backend.as_str()
    );
    Ok(restored)
}

fn switch_component(component: &mut ComponentJournal) -> Result<()> {
    if component.switched {
        verify_switched(component)?;
        return Ok(());
    }
    reject_symlink_if_present(&component.live_dir, "live data directory")?;
    reject_symlink_if_present(&component.stage_dir, "staged data directory")?;
    reject_symlink_if_present(&component.rollback_dir, "rollback data directory")?;

    let has_live = component.live_dir.exists();
    let has_stage = component.stage_dir.exists();
    let has_rollback = component.rollback_dir.exists();
    match (
        component.previous_incarnation_id.is_some(),
        has_live,
        has_stage,
        has_rollback,
    ) {
        (true, true, true, false) => {
            fs::rename(&component.live_dir, &component.rollback_dir)?;
            fs::rename(&component.stage_dir, &component.live_dir)?;
        }
        // Crash after old -> rollback, before stage -> live.
        (true, false, true, true) => fs::rename(&component.stage_dir, &component.live_dir)?,
        // Crash after both renames, before the journal flag.
        (true, true, false, true) => {}
        (false, false, true, false) => fs::rename(&component.stage_dir, &component.live_dir)?,
        (false, true, false, false) => {}
        (false, true, true, false) if directory_is_empty(&component.live_dir)? => {
            fs::remove_dir(&component.live_dir)?;
            fs::rename(&component.stage_dir, &component.live_dir)?;
        }
        state => bail!(
            "{} restore directories are not in a recoverable switch state: {:?}",
            component.backend.as_str(),
            state
        ),
    }
    sync_parent_directory(&component.live_dir)?;
    verify_switched(component)?;
    component.switched = true;
    Ok(())
}

fn verify_switched(component: &ComponentJournal) -> Result<()> {
    let expected = component
        .restored_incarnation_id
        .ok_or_else(|| anyhow!("{} has no staged incarnation", component.backend.as_str()))?;
    ensure!(
        current_incarnation(&component.live_dir)? == Some(expected),
        "{} live directory is not the staged incarnation",
        component.backend.as_str()
    );
    if component.previous_incarnation_id.is_some() {
        ensure!(
            component.rollback_dir.is_dir(),
            "{} rollback directory is missing",
            component.backend.as_str()
        );
    }
    Ok(())
}

fn verify_staged(component: &ComponentJournal) -> Result<()> {
    let expected_incarnation = component
        .restored_incarnation_id
        .ok_or_else(|| anyhow!("{} has no staged incarnation", component.backend.as_str()))?;
    let expected_digest = component
        .staged_tree_sha256
        .as_deref()
        .ok_or_else(|| anyhow!("{} has no staged tree digest", component.backend.as_str()))?;
    ensure!(
        current_incarnation(&component.stage_dir)? == Some(expected_incarnation),
        "{} staged incarnation changed",
        component.backend.as_str()
    );
    ensure!(
        tree_sha256(&component.stage_dir)? == expected_digest,
        "{} staged directory digest changed",
        component.backend.as_str()
    );
    Ok(())
}

async fn rollback(
    config: &TopologyConfig,
    journal_path: &Path,
    journal: &mut RestoreJournal,
    probe_after_start: bool,
) -> Result<()> {
    journal.phase = RestorePhase::RollingBack;
    persist_journal(journal_path, journal)?;
    for (_, service) in config.services().into_iter().rev() {
        let _ = run_command(&service.stop).await;
    }
    // A stop command may legitimately report failure when a service is
    // already gone, so listener state is the authority. Never rename a live
    // data directory until every backend is proved unreachable: doing so
    // after a partial stop would let a surviving process write through the
    // rollback switch.
    for (backend, service) in config.services() {
        wait_unreachable(service.listen).await.with_context(|| {
            format!(
                "{} remained reachable while restore rollback was preparing its directory switch",
                backend.as_str()
            )
        })?;
    }
    for index in (0..journal.components.len()).rev() {
        if !journal.components[index].switched {
            continue;
        }
        let component = &mut journal.components[index];
        rollback_component(component, journal.operation_id)?;
        component.switched = false;
        persist_journal(journal_path, journal)?;
    }
    for (_, service) in config.services() {
        run_command(&service.start).await?;
    }
    if probe_after_start {
        for (backend, service) in config.services() {
            wait_reachable(service.listen).await.with_context(|| {
                format!(
                    "{} did not become reachable after restore rollback",
                    backend.as_str()
                )
            })?;
        }
    }
    run_command(&config.writer_admission.open).await?;
    journal.phase = RestorePhase::RolledBack;
    persist_journal(journal_path, journal)?;
    Ok(())
}

fn rollback_component(component: &ComponentJournal, operation_id: CanonicalId) -> Result<()> {
    let prior = component
        .previous_incarnation_id
        .ok_or_else(|| anyhow!("{} has no rollback incarnation", component.backend.as_str()))?;
    let restored = component
        .restored_incarnation_id
        .ok_or_else(|| anyhow!("{} has no restored incarnation", component.backend.as_str()))?;
    let live = current_incarnation(&component.live_dir)?;
    let rollback = current_incarnation(&component.rollback_dir)?;
    let failed = current_incarnation(&component.failed_dir)?;

    // Crash after rollback -> live but before the journal flag: the desired
    // old incarnation is already back. Preserve the failed restore for
    // diagnosis and only finish the journal transition.
    if live == Some(prior) && rollback.is_none() {
        ensure!(
            failed.is_none() || failed == Some(restored),
            "{} failed-directory incarnation is unrelated to this restore",
            component.backend.as_str()
        );
        sync_parent_directory(&component.live_dir)?;
        return Ok(());
    }

    ensure!(
        rollback == Some(prior),
        "{} rollback directory does not contain the prior incarnation",
        component.backend.as_str()
    );
    if live == Some(restored) {
        if component.failed_dir.exists() {
            ensure!(
                failed == Some(restored),
                "{} failed-directory incarnation is unrelated to this restore",
                component.backend.as_str()
            );
            reset_temporary_directory(&component.failed_dir, operation_id, ".failed")?;
        }
        fs::rename(&component.live_dir, &component.failed_dir)?;
    } else {
        ensure!(
            live.is_none() && failed == Some(restored),
            "{} live/failed directories are not an interrupted rollback of this restore",
            component.backend.as_str()
        );
    }
    fs::rename(&component.rollback_dir, &component.live_dir)?;
    sync_parent_directory(&component.live_dir)?;
    ensure!(
        current_incarnation(&component.live_dir)? == Some(prior),
        "{} prior incarnation was not restored",
        component.backend.as_str()
    );
    Ok(())
}

async fn run_command(command: &ServiceCommand) -> Result<()> {
    let mut child = Command::new(&command.executable);
    child
        .args(&command.args)
        .env_clear()
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(command.timeout_seconds), child.status())
        .await
        .context("service command timed out")??;
    ensure!(status.success(), "service command exited with {status}");
    Ok(())
}

async fn restart_services_and_open_admission(
    config: &TopologyConfig,
    probe_after_start: bool,
) -> Result<()> {
    let mut first_start_error = None;
    for (backend, service) in config.services() {
        if let Err(error) = run_command(&service.start).await
            && first_start_error.is_none()
        {
            first_start_error =
                Some(error.context(format!("{} could not be restarted", backend.as_str())));
        }
    }
    if let Some(error) = first_start_error {
        return Err(error.context("writer admission remains closed"));
    }
    if probe_after_start {
        for (backend, service) in config.services() {
            wait_reachable(service.listen).await.with_context(|| {
                format!(
                    "{} did not become reachable; writer admission remains closed",
                    backend.as_str()
                )
            })?;
        }
    }
    run_command(&config.writer_admission.open)
        .await
        .context("could not reopen writer admission")
}

fn validate_snapshot_root(config: &TopologyConfig, out: &Path) -> Result<()> {
    ensure!(
        out.is_absolute() && path_is_lexically_clean(out),
        "snapshot-set output must be a lexically clean absolute path"
    );
    let out_text = out
        .to_str()
        .context("snapshot-set output must be valid UTF-8")?;
    ensure!(
        !out_text.chars().any(char::is_whitespace)
            && !out_text.contains('%')
            && !out_text.contains('?')
            && !out_text.contains('#'),
        "snapshot-set output cannot be represented as an unambiguous local file URI"
    );
    ensure!(
        config
            .snapshot_roots
            .iter()
            .any(|root| out != root && out.starts_with(root)),
        "snapshot-set output must be a descendant of one configured snapshot root"
    );
    reject_existing_ancestor_symlinks(out, "snapshot-set output ancestor")?;
    for protected in std::iter::once(&config.state_dir).chain(
        config
            .services()
            .into_iter()
            .map(|(_, service)| &service.data_dir),
    ) {
        ensure!(
            !out.starts_with(protected) && !protected.starts_with(out),
            "snapshot-set output and {} must not contain one another",
            protected.display()
        );
    }
    Ok(())
}

fn ensure_secure_state_dir(path: &Path) -> Result<()> {
    for ancestor in path.ancestors().filter(|candidate| candidate.exists()) {
        reject_symlink(ancestor, "topology state directory ancestor")?;
    }
    fs::create_dir_all(path)
        .with_context(|| format!("cannot create topology state dir {}", path.display()))?;
    reject_symlink(path, "topology state directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn path_is_lexically_clean(path: &Path) -> bool {
    path.components().all(|component| {
        matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Normal(_)
        )
    })
}

fn prepare_empty_directory(path: &Path, label: &str) -> Result<()> {
    match fs::read_dir(path) {
        Ok(mut entries) => {
            ensure!(
                entries.next().is_none(),
                "{} is not empty; choose a new {label}",
                path.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn persist_restore_set(path: &Path, restore_set: &BackendRestoreSet) -> Result<()> {
    ensure!(!path.exists(), "restore-set manifest already exists");
    let parent = path
        .parent()
        .context("restore-set manifest has no parent")?;
    let temporary = parent.join(format!(".restore-set-{}.tmp", restore_set.restore_set_id));
    let bytes = serde_json::to_vec_pretty(restore_set)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    sync_parent_directory(path)?;
    Ok(())
}

fn acquire_lock(path: &Path, busy_message: &str) -> Result<File> {
    reject_symlink_if_present(path, "topology operation lock")?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    lock.try_lock_exclusive()
        .context(busy_message.to_string())?;
    Ok(lock)
}

fn validate_trusted_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect {label} {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "{label} must be a regular file, not a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o022 == 0,
            "{label} must not be writable by group or other users"
        );
    }
    Ok(())
}

async fn wait_unreachable(address: SocketAddr) -> Result<()> {
    for _ in 0..100 {
        match tokio::time::timeout(Duration::from_millis(100), TcpStream::connect(address)).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => return Ok(()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("{address} is still accepting TCP connections")
}

async fn wait_reachable(address: SocketAddr) -> Result<()> {
    for _ in 0..100 {
        if let Ok(Ok(_)) =
            tokio::time::timeout(Duration::from_millis(100), TcpStream::connect(address)).await
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("{address} did not accept TCP connections")
}

fn current_incarnation(data_dir: &Path) -> Result<Option<CanonicalId>> {
    reject_symlink_if_present(data_dir, "backend data directory")?;
    if !data_dir.exists() || directory_is_empty(data_dir)? {
        return Ok(None);
    }
    let journal = data_dir.join(FENCE_JOURNAL);
    ensure!(
        journal.is_file(),
        "non-empty backend data directory {} has no {FENCE_JOURNAL}",
        data_dir.display()
    );
    Ok(Some(
        TenantWriteFences::open_blocking(journal)?.backend_incarnation_id(),
    ))
}

fn reset_temporary_directory(path: &Path, operation_id: CanonicalId, suffix: &str) -> Result<()> {
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
    ensure!(
        name.contains(&operation_id.to_string()) && name.ends_with(suffix),
        "refused to reset a directory outside this restore operation"
    );
    reject_symlink_if_present(path, "restore temporary directory")?;
    if path.exists() {
        fs::remove_dir_all(path).with_context(|| {
            format!(
                "cannot reset restore temporary directory {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn local_snapshot_path(config: &TopologyConfig, uri: &str) -> Result<PathBuf> {
    let raw = uri.strip_prefix("file://").ok_or_else(|| {
        anyhow!("embedded topology driver currently requires a file:/// snapshot URI")
    })?;
    ensure!(
        raw.starts_with('/'),
        "file snapshot URI must name an absolute path"
    );
    ensure!(
        !raw.contains('%'),
        "percent-encoded file snapshot URI is not accepted"
    );
    let path = PathBuf::from(raw);
    ensure!(
        path_is_lexically_clean(&path),
        "file snapshot URI must be lexically clean"
    );
    ensure!(
        config
            .snapshot_roots
            .iter()
            .any(|root| path != *root && path.starts_with(root)),
        "file snapshot URI is outside every configured snapshot root"
    );
    Ok(path)
}

fn validate_command(command: &ServiceCommand, action: &str, backend: RestoreBackend) -> Result<()> {
    validate_unscoped_command(command, &format!("{} {action}", backend.as_str()))
}

fn validate_unscoped_command(command: &ServiceCommand, label: &str) -> Result<()> {
    ensure!(
        command.executable.is_absolute() && path_is_lexically_clean(&command.executable),
        "{label} executable must be a lexically clean absolute path"
    );
    ensure!(
        command.timeout_seconds > 0 && command.timeout_seconds <= 300,
        "{label} timeout must be 1..=300 seconds"
    );
    ensure!(
        command
            .args
            .iter()
            .all(|arg| arg.len() <= 4096 && !arg.chars().any(char::is_control)),
        "{label} argument is invalid"
    );
    Ok(())
}

fn report(
    journal: &RestoreJournal,
    state: BackendRestoreExecutionState,
) -> Result<BackendRestoreExecutionReport> {
    let components = journal
        .components
        .iter()
        .map(|component| {
            Ok(BackendRestoredComponent {
                backend: component.backend,
                previous_incarnation_id: component.previous_incarnation_id.ok_or_else(|| {
                    anyhow!("{} has no prior incarnation", component.backend.as_str())
                })?,
                restored_incarnation_id: component.restored_incarnation_id.ok_or_else(|| {
                    anyhow!("{} has no restored incarnation", component.backend.as_str())
                })?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(BackendRestoreExecutionReport {
        schema: BACKEND_RESTORE_REPORT_SCHEMA_V1.to_string(),
        schema_version: 1,
        operation_id: journal.operation_id,
        restore_set_id: journal.restore_set_id,
        restore_set_sha256: RestoreDigest {
            algorithm: "sha256".to_string(),
            value: journal.restore_set_sha256.clone(),
        },
        state,
        components,
    })
}

fn load_journal(path: &Path) -> Result<RestoreJournal> {
    reject_symlink(path, "restore journal")?;
    let journal: RestoreJournal = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        journal.schema_version == JOURNAL_VERSION,
        "unknown restore journal schema"
    );
    ensure!(
        journal.components.len() == 3,
        "restore journal component set is incomplete"
    );
    for (index, backend) in RestoreBackend::REQUIRED.into_iter().enumerate() {
        ensure!(
            journal.components[index].backend == backend,
            "restore journal component order changed"
        );
    }
    validate_sha256(&journal.restore_set_sha256, "restore set")?;
    for component in &journal.components {
        if let Some(digest) = &component.staged_tree_sha256 {
            validate_sha256(digest, "staged tree")?;
        }
        ensure!(
            component.restored_incarnation_id.is_some() == component.staged_tree_sha256.is_some(),
            "restore journal staged incarnation and tree digest disagree"
        );
    }
    Ok(journal)
}

fn persist_journal(path: &Path, journal: &RestoreJournal) -> Result<()> {
    let parent = path.parent().context("restore journal has no parent")?;
    fs::create_dir_all(parent)?;
    reject_symlink(parent, "restore journal directory")?;
    let temporary = parent.join(format!(".{}.tmp", journal.operation_id));
    prepare_atomic_temporary(&temporary, "restore journal")?;
    let bytes = serde_json::to_vec_pretty(journal)?;
    let mut options = OpenOptions::new();
    // create_new maps to an exclusive create: a same-account process racing
    // a symlink into this deterministic name makes the write fail rather than
    // redirecting journal bytes outside the 0700 state directory.
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    sync_parent_directory(path)?;
    Ok(())
}

fn prepare_atomic_temporary(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "{label} temporary path must be a regular file"
            );
            fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn tree_bytes(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let file_type = entry.file_type()?;
            ensure!(!file_type.is_symlink(), "snapshot contains a symbolic link");
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                total = total
                    .checked_add(entry.metadata()?.len())
                    .context("snapshot byte count exceeds u64")?;
            } else {
                bail!("snapshot contains a non-file filesystem object")
            }
        }
    }
    Ok(total)
}

fn tree_sha256(path: &Path) -> Result<String> {
    let mut entries = Vec::new();
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            ensure!(
                !file_type.is_symlink(),
                "data tree contains a symbolic link"
            );
            if file_type.is_dir() {
                entries.push((entry.path(), true));
                pending.push(entry.path());
            } else if file_type.is_file() {
                entries.push((entry.path(), false));
            } else {
                bail!("data tree contains a non-file filesystem object");
            }
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    for (entry_path, is_directory) in entries {
        let relative = entry_path
            .strip_prefix(path)
            .context("data tree entry escaped its root")?;
        let relative = relative.as_os_str().as_encoded_bytes();
        digest.update([u8::from(!is_directory)]);
        digest.update((relative.len() as u64).to_be_bytes());
        digest.update(relative);
        if is_directory {
            continue;
        }
        let mut file = File::open(&entry_path)?;
        let size = file.metadata()?.len();
        digest.update(size.to_be_bytes());
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn directory_is_empty(path: &Path) -> Result<bool> {
    Ok(fs::read_dir(path)?.next().transpose()?.is_none())
}

fn reject_tree_symlinks(path: &Path) -> Result<()> {
    reject_symlink(path, "snapshot directory")?;
    let _ = tree_bytes(path)?;
    Ok(())
}

fn reject_existing_ancestor_symlinks(path: &Path, label: &str) -> Result<()> {
    for ancestor in path.ancestors().filter(|candidate| candidate.exists()) {
        reject_symlink(ancestor, label)?;
    }
    Ok(())
}

fn reject_symlink_if_present(path: &Path, label: &str) -> Result<()> {
    if path_entry_exists(path)? {
        reject_symlink(path, label)?;
    }
    Ok(())
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn reject_symlink(path: &Path, label: &str) -> Result<()> {
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "{label} must not be a symbolic link"
    );
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} digest must be lowercase SHA-256"
    );
    Ok(())
}

fn valid_driver_id(value: &str) -> bool {
    value.len() >= 3
        && value.len() <= 128
        && value.contains('.')
        && value.split(['.', '-']).all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn valid_semver(value: &str) -> bool {
    let core = value.split_once('-').map_or(value, |(core, _)| core);
    let parts = core.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (*part == "0" || !part.starts_with('0'))
        })
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use deis_wire::BACKEND_RESTORE_SET_SCHEMA_V1;
    use tempfile::TempDir;

    use super::*;

    fn command() -> ServiceCommand {
        ServiceCommand {
            executable: PathBuf::from("/bin/true"),
            args: Vec::new(),
            timeout_seconds: 5,
        }
    }

    fn service(data_dir: PathBuf, port: u16) -> ServiceConfig {
        ServiceConfig {
            data_dir,
            listen: format!("127.0.0.1:{port}").parse().unwrap(),
            stop: command(),
            start: command(),
        }
    }

    fn initialize_backend(path: &Path) -> CanonicalId {
        fs::create_dir_all(path).unwrap();
        TenantWriteFences::open_blocking(path.join(FENCE_JOURNAL))
            .unwrap()
            .backend_incarnation_id()
    }

    struct Fixture {
        _directory: TempDir,
        config: TopologyConfig,
        restore_set: BackendRestoreSet,
        prior: Vec<CanonicalId>,
    }

    fn fixture() -> Fixture {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let source_store = root.join("source-store");
        let source_queue = root.join("source-queue");
        let source_telemetry = root.join("source-telemetry");
        let source_ids = [
            initialize_backend(&source_store),
            initialize_backend(&source_queue),
            initialize_backend(&source_telemetry),
        ];
        let backup_root = root.join("backups");
        let snapshots = [
            backup_root.join("snapshot-store"),
            backup_root.join("snapshot-queue"),
            backup_root.join("snapshot-telemetry"),
        ];
        deis_store::snapshot::take(&source_store, &snapshots[0]).unwrap();
        deis_queue::snapshot::take(&source_queue, &snapshots[1]).unwrap();
        deis_telemetry::snapshot::take(&source_telemetry, &snapshots[2]).unwrap();

        let live = [
            root.join("live-store"),
            root.join("live-queue"),
            root.join("live-telemetry"),
        ];
        let prior = live
            .iter()
            .map(|path| initialize_backend(path))
            .collect::<Vec<_>>();
        let config = TopologyConfig {
            schema_version: 1,
            driver_id: "deislet.topology.test".to_string(),
            driver_version: "0.1.0-test.1".to_string(),
            distribution_sha256: "a".repeat(64),
            state_dir: root.join("state"),
            snapshot_roots: vec![backup_root],
            writer_admission: WriterAdmissionConfig {
                close: command(),
                open: command(),
            },
            store: service(live[0].clone(), 9),
            queue: service(live[1].clone(), 10),
            telemetry: service(live[2].clone(), 11),
        };
        let backends = RestoreBackend::REQUIRED;
        let components = snapshots
            .iter()
            .enumerate()
            .map(|(index, snapshot)| {
                serde_json::json!({
                    "backend": backends[index].as_str(),
                    "snapshotArtifactId": CanonicalId::new_v7(),
                    "snapshotUri": format!("file://{}", snapshot.display()),
                    "snapshotManifestDigest": {
                        "algorithm": "sha256",
                        "value": file_sha256(&snapshot.join(MANIFEST_NAME)).unwrap()
                    },
                    "sourceBackendIncarnationId": source_ids[index],
                    "sizeBytes": tree_bytes(snapshot).unwrap(),
                    "capturedAt": format!("2026-09-03T20:50:{}Z", 20 + index * 10),
                    "containsFenceJournal": false
                })
            })
            .collect::<Vec<_>>();
        let restore_set: BackendRestoreSet = serde_json::from_value(serde_json::json!({
            "$schema": BACKEND_RESTORE_SET_SCHEMA_V1,
            "schemaVersion": 1,
            "restoreSetId": CanonicalId::new_v7(),
            "scope": {
                "kind": "installation",
                "organizationId": CanonicalId::new_v7(),
                "installationId": CanonicalId::new_v7()
            },
            "consistencyBarrier": {
                "kind": "full-platform-outage",
                "barrierId": CanonicalId::new_v7(),
                "writerAdmissionClosedAt": "2026-09-03T20:50:00Z",
                "allServicesStoppedAt": "2026-09-03T20:50:10Z",
                "snapshotCompletedAt": "2026-09-03T20:51:00Z"
            },
            "topologyDriver": {
                "id": config.driver_id,
                "version": config.driver_version,
                "distributionDigest": {"algorithm": "sha256", "value": config.distribution_sha256}
            },
            "components": components,
            "createdAt": "2026-09-03T20:51:00Z"
        }))
        .unwrap();
        restore_set.validate().unwrap();
        Fixture {
            _directory: directory,
            config,
            restore_set,
            prior,
        }
    }

    #[test]
    fn switch_recovers_both_directory_rename_crash_windows() {
        let directory = TempDir::new().unwrap();
        let live = directory.path().join("live");
        let stage = directory.path().join("stage");
        let rollback = directory.path().join("rollback");
        let failed = directory.path().join("failed");
        let prior = initialize_backend(&live);
        let restored = initialize_backend(&stage);
        let staged_tree_sha256 = tree_sha256(&stage).unwrap();
        let mut component = ComponentJournal {
            backend: RestoreBackend::DeisStore,
            live_dir: live.clone(),
            stage_dir: stage,
            rollback_dir: rollback.clone(),
            failed_dir: failed,
            previous_incarnation_id: Some(prior),
            restored_incarnation_id: Some(restored),
            staged_tree_sha256: Some(staged_tree_sha256),
            switched: false,
        };

        fs::rename(&live, &rollback).unwrap();
        switch_component(&mut component).unwrap();
        assert_eq!(current_incarnation(&live).unwrap(), Some(restored));
        assert_eq!(current_incarnation(&rollback).unwrap(), Some(prior));

        component.switched = false;
        switch_component(&mut component).unwrap();
        assert!(component.switched);
        assert_eq!(current_incarnation(&live).unwrap(), Some(restored));
    }

    #[test]
    fn rollback_recovers_both_directory_rename_crash_windows() {
        for crash_after_second_rename in [false, true] {
            let directory = TempDir::new().unwrap();
            let operation_id = CanonicalId::new_v7();
            let live = directory.path().join("live");
            let rollback = directory.path().join("rollback");
            let failed = directory.path().join("failed");
            let stage = directory.path().join("stage");
            let restored = initialize_backend(&live);
            let prior = initialize_backend(&rollback);
            let component = ComponentJournal {
                backend: RestoreBackend::DeisStore,
                live_dir: live.clone(),
                stage_dir: stage,
                rollback_dir: rollback.clone(),
                failed_dir: failed.clone(),
                previous_incarnation_id: Some(prior),
                restored_incarnation_id: Some(restored),
                staged_tree_sha256: Some("a".repeat(64)),
                switched: true,
            };
            fs::rename(&live, &failed).unwrap();
            if crash_after_second_rename {
                fs::rename(&rollback, &live).unwrap();
            }
            rollback_component(&component, operation_id).unwrap();
            assert_eq!(current_incarnation(&live).unwrap(), Some(prior));
            assert_eq!(current_incarnation(&failed).unwrap(), Some(restored));
        }
    }

    #[cfg(unix)]
    #[test]
    fn journal_persistence_never_follows_a_temporary_symlink() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new().unwrap();
        let operation_id = CanonicalId::new_v7();
        let journal_path = directory
            .path()
            .join(format!("restore-{operation_id}.json"));
        let temporary = directory.path().join(format!(".{operation_id}.tmp"));
        let victim = directory.path().join("victim");
        fs::write(&victim, b"must remain unchanged").unwrap();
        symlink(&victim, &temporary).unwrap();
        let journal = RestoreJournal {
            schema_version: JOURNAL_VERSION,
            operation_id,
            restore_set_id: CanonicalId::new_v7(),
            restore_set_sha256: "a".repeat(64),
            phase: RestorePhase::Planning,
            components: Vec::new(),
        };

        let error = persist_journal(&journal_path, &journal).unwrap_err();
        assert!(error.to_string().contains("temporary path"));
        assert_eq!(fs::read(&victim).unwrap(), b"must remain unchanged");
        assert!(
            fs::symlink_metadata(&temporary)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn journal_cannot_redefine_a_trusted_backend_path() {
        let fixture = fixture();
        let operation_id = CanonicalId::new_v7();
        let mut components = fixture
            .config
            .services()
            .into_iter()
            .map(|(backend, service)| {
                let (stage_dir, rollback_dir, failed_dir) =
                    component_paths(service, operation_id, backend).unwrap();
                ComponentJournal {
                    backend,
                    live_dir: service.data_dir.clone(),
                    stage_dir,
                    rollback_dir,
                    failed_dir,
                    previous_incarnation_id: current_incarnation(&service.data_dir).unwrap(),
                    restored_incarnation_id: None,
                    staged_tree_sha256: None,
                    switched: false,
                }
            })
            .collect::<Vec<_>>();
        components[0].live_dir = fixture.config.snapshot_roots[0].join("unrelated");
        let journal = RestoreJournal {
            schema_version: JOURNAL_VERSION,
            operation_id,
            restore_set_id: fixture.restore_set.restore_set_id,
            restore_set_sha256: sha256_hex(&fixture.restore_set.canonical_json().unwrap()),
            phase: RestorePhase::Planning,
            components,
        };

        assert!(
            validate_journal_bindings(&fixture.config, &journal)
                .unwrap_err()
                .to_string()
                .contains("trusted topology config")
        );
    }

    #[cfg(unix)]
    #[test]
    fn topology_config_rejects_a_symlinked_data_ancestor() {
        use std::os::unix::fs::symlink;

        let mut fixture = fixture();
        let root = fixture.config.state_dir.parent().unwrap();
        let target = root.join("redirected-store");
        fs::create_dir(&target).unwrap();
        let link = root.join("store-link");
        symlink(&target, &link).unwrap();
        fixture.config.store.data_dir = link.join("data");
        assert!(
            fixture
                .config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("data directory ancestor")
        );
    }

    #[tokio::test]
    async fn global_topology_lock_excludes_another_restore_identity() {
        let fixture = fixture();
        ensure_secure_state_dir(&fixture.config.state_dir).unwrap();
        let _lock =
            acquire_lock(&fixture.config.state_dir.join("topology.lock"), "test lock").unwrap();
        let error = execute_restore_inner(
            &fixture.config,
            &fixture.restore_set,
            CanonicalId::new_v7(),
            false,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("another topology operation"));
        let error =
            inspect_restore_inner(&fixture.config, CanonicalId::new_v7(), false).unwrap_err();
        assert!(error.to_string().contains("another topology operation"));
    }

    #[tokio::test]
    async fn snapshot_set_is_captured_under_one_barrier_and_is_directly_restorable() {
        let fixture = fixture();
        let output = fixture.config.snapshot_roots[0].join("captured-set");
        let organization_id = CanonicalId::new_v7();
        let installation_id = CanonicalId::new_v7();
        let restore_set = capture_restore_set_inner(
            &fixture.config,
            &output,
            organization_id,
            installation_id,
            false,
            false,
        )
        .await
        .unwrap();
        let exact_retry = capture_restore_set_inner(
            &fixture.config,
            &output,
            organization_id,
            installation_id,
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(exact_retry, restore_set);
        restore_set.validate().unwrap();
        assert_eq!(restore_set.scope.organization_id, organization_id);
        assert_eq!(restore_set.scope.installation_id, installation_id);
        assert!(output.join("restore-set.json").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for (index, component) in restore_set.components.iter().enumerate() {
            assert_eq!(
                component.source_backend_incarnation_id,
                fixture.prior[index]
            );
            assert!(component.snapshot_uri.starts_with("file:///"));
            assert!(output.join(component.backend.as_str()).is_dir());
            assert!(
                !output
                    .join(component.backend.as_str())
                    .join(FENCE_JOURNAL)
                    .exists()
            );
        }

        let operation_id = CanonicalId::new_v7();
        let report = execute_restore_inner(
            &fixture.config,
            &restore_set,
            operation_id,
            false,
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(report.state, BackendRestoreExecutionState::Succeeded);
    }

    #[tokio::test]
    async fn interrupted_complete_capture_reopens_and_publishes_from_its_journal() {
        let fixture = fixture();
        let output = fixture.config.snapshot_roots[0].join("recoverable-capture");
        let restore_set = capture_restore_set_inner(
            &fixture.config,
            &output,
            CanonicalId::new_v7(),
            CanonicalId::new_v7(),
            false,
            false,
        )
        .await
        .unwrap();
        let journal_path = fixture.config.state_dir.join(SNAPSHOT_CAPTURE_JOURNAL);
        let mut journal = load_snapshot_capture_journal(&fixture.config, &journal_path).unwrap();
        journal.phase = SnapshotCapturePhase::Captured;
        fs::remove_file(output.join("restore-set.json")).unwrap();
        persist_snapshot_capture_journal(&journal_path, &journal).unwrap();

        let recovered =
            recover_interrupted_snapshot_capture_locked(&fixture.config, &journal_path, false)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(recovered, restore_set);
        assert_eq!(
            load_snapshot_capture_journal(&fixture.config, &journal_path)
                .unwrap()
                .phase,
            SnapshotCapturePhase::Completed
        );
        assert_eq!(
            serde_json::from_slice::<BackendRestoreSet>(
                &fs::read(output.join("restore-set.json")).unwrap()
            )
            .unwrap(),
            restore_set
        );
    }

    #[tokio::test]
    async fn interrupted_incomplete_capture_is_reopened_and_marked_aborted() {
        let fixture = fixture();
        ensure_secure_state_dir(&fixture.config.state_dir).unwrap();
        let output = fixture.config.snapshot_roots[0].join("partial-capture");
        prepare_empty_directory(&output, "test snapshot output").unwrap();
        fs::write(output.join("partial"), b"diagnostic evidence").unwrap();
        let journal_path = fixture.config.state_dir.join(SNAPSHOT_CAPTURE_JOURNAL);
        let journal = SnapshotCaptureJournal {
            schema_version: SNAPSHOT_CAPTURE_JOURNAL_VERSION,
            capture_id: CanonicalId::new_v7(),
            output_dir: output.clone(),
            organization_id: CanonicalId::new_v7(),
            installation_id: CanonicalId::new_v7(),
            phase: SnapshotCapturePhase::Stopped,
            restore_set: None,
            last_error: None,
        };
        persist_snapshot_capture_journal(&journal_path, &journal).unwrap();

        assert!(
            recover_interrupted_snapshot_capture_locked(&fixture.config, &journal_path, false,)
                .await
                .unwrap()
                .is_none()
        );
        let recovered = load_snapshot_capture_journal(&fixture.config, &journal_path).unwrap();
        assert_eq!(recovered.phase, SnapshotCapturePhase::Aborted);
        assert!(recovered.last_error.unwrap().contains("partial output"));
        assert_eq!(
            fs::read(output.join("partial")).unwrap(),
            b"diagnostic evidence"
        );
        assert!(!output.join("restore-set.json").exists());
    }

    #[tokio::test]
    async fn snapshot_recovery_failure_stays_nonterminal_until_retried() {
        let mut fixture = fixture();
        ensure_secure_state_dir(&fixture.config.state_dir).unwrap();
        let output = fixture.config.snapshot_roots[0].join("retry-capture-recovery");
        prepare_empty_directory(&output, "test snapshot output").unwrap();
        let journal_path = fixture.config.state_dir.join(SNAPSHOT_CAPTURE_JOURNAL);
        let journal = SnapshotCaptureJournal {
            schema_version: SNAPSHOT_CAPTURE_JOURNAL_VERSION,
            capture_id: CanonicalId::new_v7(),
            output_dir: output,
            organization_id: CanonicalId::new_v7(),
            installation_id: CanonicalId::new_v7(),
            phase: SnapshotCapturePhase::AdmissionClosed,
            restore_set: None,
            last_error: None,
        };
        persist_snapshot_capture_journal(&journal_path, &journal).unwrap();
        fixture.config.store.start.executable = PathBuf::from("/bin/false");
        assert!(
            recover_interrupted_snapshot_capture_locked(&fixture.config, &journal_path, false,)
                .await
                .unwrap_err()
                .to_string()
                .contains("recover services")
        );
        assert_eq!(
            load_snapshot_capture_journal(&fixture.config, &journal_path)
                .unwrap()
                .phase,
            SnapshotCapturePhase::AdmissionClosed
        );

        fixture.config.store.start = command();
        assert!(
            recover_interrupted_snapshot_capture_locked(&fixture.config, &journal_path, false,)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            load_snapshot_capture_journal(&fixture.config, &journal_path)
                .unwrap()
                .phase,
            SnapshotCapturePhase::Aborted
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_broken_capture_journal_symlink_cannot_hide_recovery() {
        use std::os::unix::fs::symlink;

        let fixture = fixture();
        ensure_secure_state_dir(&fixture.config.state_dir).unwrap();
        let journal_path = fixture.config.state_dir.join(SNAPSHOT_CAPTURE_JOURNAL);
        symlink(
            fixture.config.state_dir.join("missing-journal-target"),
            &journal_path,
        )
        .unwrap();

        let error =
            recover_interrupted_snapshot_capture_locked(&fixture.config, &journal_path, false)
                .await
                .unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
    }

    #[tokio::test]
    async fn all_three_backends_switch_and_exact_retry_is_idempotent() {
        let fixture = fixture();
        let operation_id = CanonicalId::new_v7();
        let first = execute_restore_inner(
            &fixture.config,
            &fixture.restore_set,
            operation_id,
            false,
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(first.state, BackendRestoreExecutionState::Succeeded);
        for (index, (backend, service)) in fixture.config.services().into_iter().enumerate() {
            assert_eq!(first.components[index].backend, backend);
            assert_eq!(
                first.components[index].previous_incarnation_id,
                fixture.prior[index]
            );
            assert_ne!(
                first.components[index].restored_incarnation_id,
                fixture.prior[index]
            );
            assert_eq!(
                current_incarnation(&service.data_dir).unwrap(),
                Some(first.components[index].restored_incarnation_id)
            );
        }

        let retry = execute_restore_inner(
            &fixture.config,
            &fixture.restore_set,
            operation_id,
            false,
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(retry, first);
    }

    #[tokio::test]
    async fn post_switch_failure_restores_every_prior_incarnation() {
        let fixture = fixture();
        let operation_id = CanonicalId::new_v7();
        let error = execute_restore_inner(
            &fixture.config,
            &fixture.restore_set,
            operation_id,
            false,
            true,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("prior directories were restored")
        );
        for (index, (_, service)) in fixture.config.services().into_iter().enumerate() {
            assert_eq!(
                current_incarnation(&service.data_dir).unwrap(),
                Some(fixture.prior[index])
            );
        }
        let journal = load_journal(
            &fixture
                .config
                .state_dir
                .join(format!("restore-{operation_id}.json")),
        )
        .unwrap();
        assert_eq!(journal.phase, RestorePhase::RolledBack);
        let terminal = inspect_restore_inner(&fixture.config, operation_id, false)
            .unwrap()
            .unwrap();
        assert_eq!(terminal.state, BackendRestoreExecutionState::RolledBack);
        assert_eq!(terminal.operation_id, operation_id);
        assert_eq!(terminal.components.len(), 3);
    }

    #[tokio::test]
    async fn pre_switch_failure_preserves_recovery_failure_and_restarts_from_barrier() {
        let mut fixture = fixture();
        fixture.config.queue.stop.executable = PathBuf::from("/bin/false");
        fixture.config.store.start.executable = PathBuf::from("/bin/false");
        let operation_id = CanonicalId::new_v7();
        let error = execute_restore_inner(
            &fixture.config,
            &fixture.restore_set,
            operation_id,
            false,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("platform recovery also failed"));
        for (index, (_, service)) in fixture.config.services().into_iter().enumerate() {
            assert_eq!(
                current_incarnation(&service.data_dir).unwrap(),
                Some(fixture.prior[index])
            );
        }
        let journal = load_journal(
            &fixture
                .config
                .state_dir
                .join(format!("restore-{operation_id}.json")),
        )
        .unwrap();
        assert_eq!(journal.phase, RestorePhase::Staged);
    }

    #[tokio::test]
    async fn rollback_never_switches_directories_while_a_backend_is_reachable() {
        let mut fixture = fixture();
        let operation_id = CanonicalId::new_v7();
        let mut components = Vec::new();
        for (backend, service) in fixture.config.services() {
            let (stage, rollback, failed) =
                component_paths(service, operation_id, backend).unwrap();
            let restored = initialize_backend(&stage);
            let staged_tree_sha256 = tree_sha256(&stage).unwrap();
            let mut component = ComponentJournal {
                backend,
                live_dir: service.data_dir.clone(),
                stage_dir: stage,
                rollback_dir: rollback,
                failed_dir: failed,
                previous_incarnation_id: current_incarnation(&service.data_dir).unwrap(),
                restored_incarnation_id: Some(restored),
                staged_tree_sha256: Some(staged_tree_sha256),
                switched: false,
            };
            switch_component(&mut component).unwrap();
            components.push(component);
        }
        ensure_secure_state_dir(&fixture.config.state_dir).unwrap();
        let journal_path = fixture
            .config
            .state_dir
            .join(format!("restore-{operation_id}.json"));
        let mut journal = RestoreJournal {
            schema_version: JOURNAL_VERSION,
            operation_id,
            restore_set_id: fixture.restore_set.restore_set_id,
            restore_set_sha256: sha256_hex(&fixture.restore_set.canonical_json().unwrap()),
            phase: RestorePhase::Switched,
            components,
        };
        persist_journal(&journal_path, &journal).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        fixture.config.store.listen = listener.local_addr().unwrap();
        let store_live = fixture.config.store.data_dir.clone();
        let restored_store = journal.components[0].restored_incarnation_id;
        {
            let rollback_future = rollback(&fixture.config, &journal_path, &mut journal, false);
            tokio::pin!(rollback_future);
            tokio::select! {
                result = &mut rollback_future => panic!("rollback switched while service was reachable: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(300)) => {}
            }
            assert_eq!(current_incarnation(&store_live).unwrap(), restored_store);

            drop(listener);
            rollback_future.as_mut().await.unwrap();
        }
        assert_eq!(
            current_incarnation(&store_live).unwrap(),
            Some(fixture.prior[0])
        );
        assert_eq!(journal.phase, RestorePhase::RolledBack);
    }

    #[tokio::test]
    async fn manifest_tamper_fails_before_live_data_is_stopped_or_moved() {
        let mut fixture = fixture();
        fixture.restore_set.components[0]
            .snapshot_manifest_digest
            .value = "f".repeat(64);
        let operation_id = CanonicalId::new_v7();
        let error = execute_restore_inner(
            &fixture.config,
            &fixture.restore_set,
            operation_id,
            false,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("manifest digest"));
        for (index, (_, service)) in fixture.config.services().into_iter().enumerate() {
            assert_eq!(
                current_incarnation(&service.data_dir).unwrap(),
                Some(fixture.prior[index])
            );
        }
        assert!(
            inspect_restore_inner(&fixture.config, operation_id, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn local_snapshot_paths_are_confined_to_clean_configured_roots() {
        let fixture = fixture();
        let outside = fixture.config.snapshot_roots[0]
            .parent()
            .unwrap()
            .join("source-store");
        let error = local_snapshot_path(&fixture.config, &format!("file://{}", outside.display()))
            .unwrap_err();
        assert!(error.to_string().contains("outside every configured"));

        let traversed = fixture.config.snapshot_roots[0]
            .join("set")
            .join("..")
            .join("snapshot-store");
        let error =
            local_snapshot_path(&fixture.config, &format!("file://{}", traversed.display()))
                .unwrap_err();
        assert!(error.to_string().contains("lexically clean"));
    }

    #[test]
    fn staged_tree_digest_detects_changes_that_preserve_the_incarnation() {
        let fixture = fixture();
        let operation_id = CanonicalId::new_v7();
        let (stage_dir, rollback_dir, failed_dir) = component_paths(
            &fixture.config.store,
            operation_id,
            RestoreBackend::DeisStore,
        )
        .unwrap();
        let restored = stage_component(
            &fixture.config,
            &fixture.restore_set.components[0],
            &stage_dir,
        )
        .unwrap();
        let component = ComponentJournal {
            backend: RestoreBackend::DeisStore,
            live_dir: fixture.config.store.data_dir.clone(),
            stage_dir: stage_dir.clone(),
            rollback_dir,
            failed_dir,
            previous_incarnation_id: Some(fixture.prior[0]),
            restored_incarnation_id: Some(restored),
            staged_tree_sha256: Some(tree_sha256(&stage_dir).unwrap()),
            switched: false,
        };
        verify_staged(&component).unwrap();
        fs::write(stage_dir.join("unrecorded-data"), b"changed after staging").unwrap();
        assert!(
            verify_staged(&component)
                .unwrap_err()
                .to_string()
                .contains("digest changed")
        );
    }
}
