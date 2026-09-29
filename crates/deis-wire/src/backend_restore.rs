//! Immutable input to a coordinated restore of Deislet's embedded backends.
//!
//! A store, queue or telemetry snapshot is internally consistent, but three
//! independently-timed snapshots are not automatically one consistent
//! platform recovery point.  This type admits a set only when all three were
//! captured after the same full-platform outage barrier.  A topology driver
//! can then stage and switch them, and each restored directory starts a new
//! backend incarnation rather than claiming its source's.

use std::{collections::BTreeSet, fmt};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::CanonicalId;

const JCS_SAFE_INTEGER_MAX: u64 = 9_007_199_254_740_991;

pub const BACKEND_RESTORE_SET_SCHEMA_V1: &str =
    "https://github.com/deislet/deislet/contracts/v1/backend-restore-set.schema.json";
pub const BACKEND_RESTORE_REPORT_SCHEMA_V1: &str =
    "https://github.com/deislet/deislet/contracts/v1/backend-restore-report.schema.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendRestoreSet {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub schema_version: u32,
    pub restore_set_id: CanonicalId,
    pub scope: RestoreScope,
    pub consistency_barrier: RestoreConsistencyBarrier,
    pub topology_driver: RestoreTopologyDriver,
    pub components: Vec<RestoreComponent>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreScope {
    pub kind: RestoreScopeKind,
    pub organization_id: CanonicalId,
    pub installation_id: CanonicalId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreScopeKind {
    Installation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreConsistencyBarrier {
    pub kind: RestoreBarrierKind,
    pub barrier_id: CanonicalId,
    pub writer_admission_closed_at: DateTime<Utc>,
    pub all_services_stopped_at: DateTime<Utc>,
    pub snapshot_completed_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreBarrierKind {
    FullPlatformOutage,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreTopologyDriver {
    pub id: String,
    pub version: String,
    pub distribution_digest: RestoreDigest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreComponent {
    pub backend: RestoreBackend,
    pub snapshot_artifact_id: CanonicalId,
    pub snapshot_uri: String,
    pub snapshot_manifest_digest: RestoreDigest,
    pub source_backend_incarnation_id: CanonicalId,
    pub size_bytes: u64,
    pub captured_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreBackend {
    DeisStore,
    DeisQueue,
    DeisTelemetry,
}

impl RestoreBackend {
    pub const REQUIRED: [Self; 3] = [Self::DeisStore, Self::DeisQueue, Self::DeisTelemetry];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeisStore => "deis-store",
            Self::DeisQueue => "deis-queue",
            Self::DeisTelemetry => "deis-telemetry",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreDigest {
    pub algorithm: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendRestoreExecutionReport {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub schema_version: u32,
    pub operation_id: CanonicalId,
    pub restore_set_id: CanonicalId,
    pub restore_set_sha256: RestoreDigest,
    pub state: BackendRestoreExecutionState,
    pub components: Vec<BackendRestoredComponent>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendRestoreExecutionState {
    Succeeded,
    RolledBack,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendRestoredComponent {
    pub backend: RestoreBackend,
    pub previous_incarnation_id: CanonicalId,
    pub restored_incarnation_id: CanonicalId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendRestoreSetError(String);

impl fmt::Display for BackendRestoreSetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BackendRestoreSetError {}

impl BackendRestoreSet {
    /// Check invariants JSON Schema cannot express, before a restore request
    /// is made durable or handed to a topology driver.
    pub fn validate(&self) -> Result<(), BackendRestoreSetError> {
        require(
            self.schema == BACKEND_RESTORE_SET_SCHEMA_V1,
            "restore set uses an unknown schema",
        )?;
        require(
            self.schema_version == 1,
            "restore set schemaVersion must be 1",
        )?;
        require(
            valid_driver_id(&self.topology_driver.id),
            "restore set topology driver ID is not canonical",
        )?;
        require(
            valid_semver(&self.topology_driver.version),
            "restore set topology driver version is not pinned semantic version text",
        )?;
        validate_digest(
            &self.topology_driver.distribution_digest,
            "topology driver distribution",
        )?;

        let barrier = &self.consistency_barrier;
        require(
            barrier.writer_admission_closed_at <= barrier.all_services_stopped_at
                && barrier.all_services_stopped_at <= barrier.snapshot_completed_at
                && barrier.snapshot_completed_at <= self.created_at,
            "restore set outage barrier timestamps are not monotonic",
        )?;
        require(
            self.components.len() == RestoreBackend::REQUIRED.len(),
            "restore set must contain store, queue and telemetry exactly once",
        )?;

        let mut artifacts = BTreeSet::new();
        let mut incarnations = BTreeSet::new();
        for (index, component) in self.components.iter().enumerate() {
            require(
                component.backend == RestoreBackend::REQUIRED[index],
                "restore set components must use canonical store, queue, telemetry order",
            )?;
            require(
                artifacts.insert(component.snapshot_artifact_id),
                "restore set repeats a snapshot artifact identity",
            )?;
            require(
                incarnations.insert(component.source_backend_incarnation_id),
                "restore set repeats a source backend incarnation identity",
            )?;
            require(
                component.size_bytes > 0 && component.size_bytes <= JCS_SAFE_INTEGER_MAX,
                "restore set snapshot size must be a positive JCS/I-JSON safe integer",
            )?;
            require(
                component.captured_at >= barrier.all_services_stopped_at
                    && component.captured_at <= barrier.snapshot_completed_at,
                "restore set component was not captured inside the outage barrier",
            )?;
            validate_snapshot_uri(&component.snapshot_uri)?;
            validate_digest(&component.snapshot_manifest_digest, "snapshot manifest")?;
        }
        Ok(())
    }

    /// RFC 8785 JSON Canonicalization Scheme bytes used by every platform for
    /// the restore request fingerprint and execution-report binding.
    pub fn canonical_json(&self) -> Result<Vec<u8>, BackendRestoreSetError> {
        self.validate()?;
        serde_jcs::to_vec(self).map_err(|error| {
            BackendRestoreSetError(format!("could not serialize valid restore set: {error}"))
        })
    }
}

fn validate_digest(digest: &RestoreDigest, label: &str) -> Result<(), BackendRestoreSetError> {
    require(
        digest.algorithm == "sha256"
            && digest.value.len() == 64
            && digest
                .value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        &format!("{label} digest must be lowercase SHA-256"),
    )
}

fn validate_snapshot_uri(uri: &str) -> Result<(), BackendRestoreSetError> {
    require(
        (8..=2048).contains(&uri.len())
            && !uri.chars().any(char::is_whitespace)
            && !uri.contains('?')
            && !uri.contains('#'),
        "restore set snapshot URI has an invalid length, whitespace, query, or fragment",
    )?;
    if uri.starts_with("file:///") {
        return Ok(());
    }
    let Some(authority_and_path) = uri.strip_prefix("https://") else {
        return Err(BackendRestoreSetError(
            "restore set snapshot URI must use file:/// or https://".to_string(),
        ));
    };
    let authority = authority_and_path.split('/').next().unwrap_or_default();
    require(
        !authority.is_empty() && !authority.contains('@') && authority_and_path.contains('/'),
        "restore set HTTPS snapshot URI must not embed credentials and must name an object",
    )
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

fn require(condition: bool, message: &str) -> Result<(), BackendRestoreSetError> {
    if condition {
        Ok(())
    } else {
        Err(BackendRestoreSetError(message.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    fn valid_set() -> BackendRestoreSet {
        let mut value: serde_json::Value = serde_json::from_str(include_str!(
            "../../../contracts/v1/fixtures/valid/backend-restore-set.json"
        ))
        .unwrap();
        value["$schema"] = BACKEND_RESTORE_SET_SCHEMA_V1.into();
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn contract_fixture_round_trips_and_validates() {
        let restore_set = valid_set();
        restore_set.validate().unwrap();
        let bytes = restore_set.canonical_json().unwrap();
        assert_eq!(
            serde_json::from_slice::<BackendRestoreSet>(&bytes).unwrap(),
            restore_set
        );
    }

    #[test]
    fn execution_report_contract_fixture_round_trips() {
        let report: BackendRestoreExecutionReport = serde_json::from_str(include_str!(
            "../../../contracts/v1/fixtures/valid/backend-restore-report.json"
        ))
        .unwrap();
        assert_eq!(report.schema, BACKEND_RESTORE_REPORT_SCHEMA_V1);
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.components.len(), 3);
        assert_eq!(report.components[0].backend, RestoreBackend::DeisStore);
        assert_eq!(report.components[1].backend, RestoreBackend::DeisQueue);
        assert_eq!(report.components[2].backend, RestoreBackend::DeisTelemetry);
        assert_eq!(
            report.restore_set_sha256.value,
            format!(
                "{:x}",
                Sha256::digest(valid_set().canonical_json().unwrap())
            )
        );
        assert_eq!(
            serde_json::from_slice::<BackendRestoreExecutionReport>(
                &serde_json::to_vec(&report).unwrap()
            )
            .unwrap(),
            report
        );
    }

    #[test]
    fn independently_timed_or_incomplete_snapshots_are_rejected() {
        let mut restore_set = valid_set();
        restore_set.components[0].captured_at =
            restore_set.consistency_barrier.all_services_stopped_at - chrono::Duration::seconds(1);
        assert!(
            restore_set
                .validate()
                .unwrap_err()
                .to_string()
                .contains("outage barrier")
        );

        let mut restore_set = valid_set();
        restore_set.components.pop();
        assert!(
            restore_set
                .validate()
                .unwrap_err()
                .to_string()
                .contains("exactly once")
        );

        let mut restore_set = valid_set();
        restore_set.components[0].size_bytes = JCS_SAFE_INTEGER_MAX + 1;
        assert!(
            restore_set
                .validate()
                .unwrap_err()
                .to_string()
                .contains("safe integer")
        );
    }

    #[test]
    fn credentials_cannot_enter_restore_input() {
        let mut restore_set = valid_set();
        restore_set.components[0].snapshot_uri =
            "https://token@example.invalid/store.snapshot".to_string();
        assert!(
            restore_set
                .validate()
                .unwrap_err()
                .to_string()
                .contains("credentials")
        );

        let mut restore_set = valid_set();
        restore_set.components[0].snapshot_uri =
            "https://example.invalid/store.snapshot?token=secret".to_string();
        assert!(
            restore_set
                .validate()
                .unwrap_err()
                .to_string()
                .contains("query")
        );
    }
}
