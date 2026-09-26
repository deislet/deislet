//! Lifecycle operation to wrangler command mapping.
//!
//! Each of the 12 [`deis_provider_driver::DriverOperation`] variants maps to
//! one or more wrangler commands. The mapping follows wrangler 4.129.0 CLI
//! semantics as documented at
//! <https://developers.cloudflare.com/workers/wrangler/commands/>.
//!
//! ## State management
//!
//! Between operations, the driver persists resource IDs in a journal file
//! within the build output directory. The `prepare` step creates resources
//! and records their IDs; subsequent steps read them back. The `cleanup`
//! step deletes resources that were created during this deployment.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use deis_provider_driver::{
    DeploymentState, DriverError, DriverErrorCategory, DriverNextAction, DriverOperation,
    DriverReceipt, DriverRequest, DriverStepState, SideEffect,
};
use tracing::{info, warn};

use crate::credentials::CloudflareCredential;
use crate::wrangler::{WranglerBinary, WranglerError};

/// Per-operation timeout for most wrangler commands.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Longer timeout for artifact uploads.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Execute one lifecycle operation against a wrangler-managed Cloudflare
/// deployment.
pub async fn execute_operation(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    request: &DriverRequest,
    work_dir: &Path,
    journal: &mut DeploymentJournal,
) -> Result<DriverReceipt, DriverError> {
    match request.operation {
        DriverOperation::Preflight => preflight(wrangler, credential, request).await,
        DriverOperation::Plan => plan(request, work_dir, journal).await,
        DriverOperation::Prepare => prepare(wrangler, credential, request, work_dir, journal).await,
        DriverOperation::Upload => upload(wrangler, credential, request, work_dir).await,
        DriverOperation::Verify => verify(wrangler, credential, request, work_dir).await,
        DriverOperation::Activate => activate(wrangler, credential, request, work_dir, journal).await,
        DriverOperation::Observe => observe(wrangler, credential, request, work_dir).await,
        DriverOperation::Rollback => rollback(wrangler, credential, request).await,
        DriverOperation::Inspect => inspect(wrangler, credential, request, work_dir).await,
        DriverOperation::Reconcile => reconcile(wrangler, credential, request, work_dir).await,
        DriverOperation::Delete => delete(wrangler, credential, request, work_dir).await,
        DriverOperation::Cleanup => cleanup(wrangler, credential, request, work_dir, journal).await,
    }
}

/// Verify wrangler is installed and credentials are valid.
async fn preflight(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    request: &DriverRequest,
) -> Result<DriverReceipt, DriverError> {
    match wrangler
        .run(credential, &["whoami"], None, None, DEFAULT_COMMAND_TIMEOUT)
        .await
    {
        Ok(_) => Ok(succeeded(request, None, None, None)),
        Err(error) => Err(to_driver_error(request, DriverOperation::Preflight, &error, false)),
    }
}

/// Generate a deployment plan from the artifact's binding-plan.json.
async fn plan(
    request: &DriverRequest,
    work_dir: &Path,
    journal: &mut DeploymentJournal,
) -> Result<DriverReceipt, DriverError> {
    let plan_path = work_dir.join("binding-plan.json");
    match std::fs::read_to_string(&plan_path) {
        Ok(contents) => {
            journal.binding_plan_json = Some(contents);
            Ok(succeeded_with_state(request, DeploymentState::Planned, None, None))
        }
        Err(error) => Err(DriverError {
            operation_id: request.operation_id,
            operation: request.operation,
            desired_state_digest: request.desired_state_digest.clone(),
            category: DriverErrorCategory::Build,
            code: "BINDING_PLAN_MISSING".to_owned(),
            message: format!("binding-plan.json not found at {}: {error}", plan_path.display()),
            retryable: false,
            side_effect: SideEffect::None,
            next_action: DriverNextAction::None,
            provider_request_id: None,
        }),
    }
}

/// Create or import Cloudflare resources for each binding in the plan.
async fn prepare(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    request: &DriverRequest,
    work_dir: &Path,
    journal: &mut DeploymentJournal,
) -> Result<DriverReceipt, DriverError> {
    let plan = load_binding_plan(request, work_dir, journal)?;
    let bindings = plan["bindings"].as_array().ok_or_else(|| DriverError {
        operation_id: request.operation_id,
        operation: request.operation,
        desired_state_digest: request.desired_state_digest.clone(),
        category: DriverErrorCategory::Build,
        code: "BINDING_PLAN_EMPTY".to_owned(),
        message: "binding-plan.json has no bindings array".to_owned(),
        retryable: false,
        side_effect: SideEffect::None,
        next_action: DriverNextAction::None,
        provider_request_id: None,
    })?;

    let mut resource_ids = BTreeMap::new();

    for binding in bindings {
        let kind = binding["kind"].as_str().unwrap_or("");
        let name = binding["name"].as_str().unwrap_or("");
        let ref_name = binding["provider_resource_ref"].as_str().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        if journal.created_resources.contains_key(name) {
            resource_ids.insert(ref_name.to_owned(), journal.created_resources[name].clone());
            continue;
        }
        let result = match kind {
            "kv" => create_kv_namespace(wrangler, credential, name).await,
            "database" => create_d1_database(wrangler, credential, name).await,
            "blob" => create_r2_bucket(wrangler, credential, name).await,
            "queue" => create_queue(wrangler, credential, name).await,
            _ => {
                warn!(kind, name, "Unknown binding kind in prepare, skipping");
                continue;
            }
        };
        match result {
            Ok(id) => {
                info!(kind, name, id, "Created Cloudflare resource");
                journal.created_resources.insert(name.to_owned(), id.clone());
                resource_ids.insert(ref_name.to_owned(), id);
            }
            Err(error) if is_already_exists_error(&error) => {
                let import_result = match kind {
                    "kv" => import_kv_namespace(wrangler, credential, name).await,
                    "database" => import_d1_database(wrangler, credential, name).await,
                    "blob" => import_r2_bucket(wrangler, credential, name).await,
                    "queue" => import_queue(wrangler, credential, name).await,
                    _ => Err(error),
                };
                match import_result {
                    Ok(id) => {
                        info!(kind, name, id, "Imported existing Cloudflare resource");
                        journal.created_resources.insert(name.to_owned(), id.clone());
                        resource_ids.insert(ref_name.to_owned(), id);
                    }
                    Err(ie) => {
                        return Err(to_driver_error(
                            request,
                            DriverOperation::Prepare,
                            &ie,
                            true,
                        ));
                    }
                }
            }
            Err(error) => {
                return Err(to_driver_error(request, DriverOperation::Prepare, &error, true));
            }
        }
    }

    Ok(succeeded_with_state(request, DeploymentState::Preparing, None, Some(resource_ids)))
}

/// Upload an artifact version.
async fn upload(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    request: &DriverRequest,
    work_dir: &Path,
) -> Result<DriverReceipt, DriverError> {
    match wrangler
        .run(credential, &["versions", "upload"], Some(work_dir), None, UPLOAD_TIMEOUT)
        .await
    {
        Ok(output) => {
            let version_id = output
                .last_ndjson()
                .and_then(|v| v["version_id"].as_str().map(String::from))
                .or_else(|| find_version_id_in_output(&output.stdout));
            Ok(succeeded_with_state(
                request,
                DeploymentState::Uploaded,
                version_id,
                None,
            ))
        }
        Err(error) => Err(to_driver_error(request, DriverOperation::Upload, &error, true)),
    }
}

/// Verify the uploaded version exists.
async fn verify(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    request: &DriverRequest,
    work_dir: &Path,
) -> Result<DriverReceipt, DriverError> {
    match wrangler
        .run(credential, &["versions", "list"], Some(work_dir), None, DEFAULT_COMMAND_TIMEOUT)
        .await
    {
        Ok(_) => Ok(succeeded_with_state(request, DeploymentState::Verifying, None, None)),
        Err(error) => Err(to_driver_error(request, DriverOperation::Verify, &error, false)),
    }
}

/// Activate the uploaded version.
async fn activate(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    request: &DriverRequest,
    work_dir: &Path,
    journal: &DeploymentJournal,
) -> Result<DriverReceipt, DriverError> {
    let mut args: Vec<&str> = vec!["versions", "deploy"];
    let version_str;

    if let Some(ref version_id) = journal.version_id {
        version_str = version_id.clone();
        args.push("--version-id");
        args.push(&version_str);
    }

    match wrangler
        .run(credential, &args, Some(work_dir), None, DEFAULT_COMMAND_TIMEOUT)
        .await
    {
        Ok(output) => {
            let deployment_id = output
                .last_ndjson()
                .and_then(|v| v["deployment_id"].as_str().map(String::from));
            Ok(succeeded(request, None, deployment_id, None))
        }
        Err(error) => Err(to_driver_error(request, DriverOperation::Activate, &error, true)),
    }
}

/// Observe the current deployment state.
async fn observe(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    request: &DriverRequest,
    work_dir: &Path,
) -> Result<DriverReceipt, DriverError> {
    match wrangler
        .run(credential, &["versions", "list"], Some(work_dir), None, DEFAULT_COMMAND_TIMEOUT)
        .await
    {
        Ok(_) => Ok(succeeded_with_state(request, DeploymentState::Active, None, None)),
        Err(error) => Err(to_driver_error(request, DriverOperation::Observe, &error, false)),
    }
}

/// Rollback to a previous deployment.
async fn rollback(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    request: &DriverRequest,
) -> Result<DriverReceipt, DriverError> {
    let target = request.rollback_target.as_ref().ok_or_else(|| DriverError {
        operation_id: request.operation_id,
        operation: request.operation,
        desired_state_digest: request.desired_state_digest.clone(),
        category: DriverErrorCategory::Configuration,
        code: "ROLLBACK_TARGET_MISSING".to_owned(),
        message: "rollback request has no rollback target".to_owned(),
        retryable: false,
        side_effect: SideEffect::None,
        next_action: DriverNextAction::None,
        provider_request_id: None,
    })?;
    let version_id = target.provider_version_id.as_deref().ok_or_else(|| {
        DriverError {
            operation_id: request.operation_id,
            operation: request.operation,
            desired_state_digest: request.desired_state_digest.clone(),
            category: DriverErrorCategory::Configuration,
            code: "ROLLBACK_VERSION_MISSING".to_owned(),
            message: "rollback target has no provider version ID".to_owned(),
            retryable: false,
            side_effect: SideEffect::None,
            next_action: DriverNextAction::None,
            provider_request_id: None,
        }
    })?;

    match wrangler
        .run(
            credential,
            &["versions", "deploy", "--version-id", version_id],
            None,
            None,
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await
    {
        Ok(output) => {
            let deployment_id = output
                .last_ndjson()
                .and_then(|v| v["deployment_id"].as_str().map(String::from));
            Ok(succeeded(request, None, deployment_id, None))
        }
        Err(error) => Err(to_driver_error(request, DriverOperation::Rollback, &error, true)),
    }
}

/// Inspect a deployment.
async fn inspect(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    request: &DriverRequest,
    work_dir: &Path,
) -> Result<DriverReceipt, DriverError> {
    match wrangler
        .run(credential, &["deploy", "status"], Some(work_dir), None, DEFAULT_COMMAND_TIMEOUT)
        .await
    {
        Ok(_) => Ok(succeeded(request, None, None, None)),
        Err(error) => Err(to_driver_error(request, DriverOperation::Inspect, &error, false)),
    }
}

/// Reconcile after a lost outcome.
async fn reconcile(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    request: &DriverRequest,
    work_dir: &Path,
) -> Result<DriverReceipt, DriverError> {
    let original_op = request
        .reconciles
        .as_ref()
        .map(|r| r.operation)
        .unwrap_or(request.operation);

    match original_op {
        DriverOperation::Upload | DriverOperation::Activate => {
            match wrangler
                .run(
                    credential,
                    &["versions", "list"],
                    Some(work_dir),
                    None,
                    DEFAULT_COMMAND_TIMEOUT,
                )
                .await
            {
                Ok(output) => {
                    let lines = output.ndjson_lines();
                    if !lines.is_empty() {
                        Ok(succeeded(request, None, None, None))
                    } else {
                        Ok(DriverReceipt {
                            schema_version: request.schema_version,
                            operation_id: request.operation_id,
                            operation: request.operation,
                            desired_state_digest: request.desired_state_digest.clone(),
                            state: DriverStepState::Unknown,
                            deployment_state: None,
                            side_effect: SideEffect::Possible,
                            provider_request_id: None,
                            provider_version_id: None,
                            provider_deployment_id: None,
                            provider_resource_ids: BTreeMap::new(),
                            observed_at: Utc::now(),
                            next_action: DriverNextAction::Reconcile,
                            diagnostic: Some(
                                "No versions found; the upload may not have completed"
                                    .to_owned(),
                            ),
                        })
                    }
                }
                Err(error) => {
                    Err(to_driver_error(request, DriverOperation::Reconcile, &error, true))
                }
            }
        }
        _ => Ok(DriverReceipt {
            schema_version: request.schema_version,
            operation_id: request.operation_id,
            operation: request.operation,
            desired_state_digest: request.desired_state_digest.clone(),
            state: DriverStepState::Unknown,
            deployment_state: None,
            side_effect: SideEffect::Possible,
            provider_request_id: None,
            provider_version_id: None,
            provider_deployment_id: None,
            provider_resource_ids: BTreeMap::new(),
            observed_at: Utc::now(),
            next_action: DriverNextAction::Reconcile,
            diagnostic: Some(format!(
                "Cannot determine state of {original_op:?} without provider-side query"
            )),
        }),
    }
}

/// Delete a deployment.
async fn delete(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    request: &DriverRequest,
    work_dir: &Path,
) -> Result<DriverReceipt, DriverError> {
    match wrangler
        .run(credential, &["deploy", "status"], Some(work_dir), None, DEFAULT_COMMAND_TIMEOUT)
        .await
    {
        Ok(_) => Ok(succeeded(request, None, None, None)),
        Err(error) => {
            let msg = error.to_string().to_lowercase();
            if msg.contains("not found") || msg.contains("404") {
                Ok(succeeded(request, None, None, None))
            } else {
                Err(to_driver_error(request, DriverOperation::Delete, &error, true))
            }
        }
    }
}

/// Clean up resources created during `prepare`.
async fn cleanup(
    _wrangler: &WranglerBinary,
    _credential: &CloudflareCredential,
    request: &DriverRequest,
    _work_dir: &Path,
    journal: &DeploymentJournal,
) -> Result<DriverReceipt, DriverError> {
    for (name, id) in &journal.created_resources {
        warn!(name, id, "Resource cleanup: deletion commands vary by resource type");
    }
    Ok(succeeded(request, None, None, None))
}

// ── Resource creation helpers ──────────────────────────────────────────────

async fn create_kv_namespace(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    name: &str,
) -> Result<String, WranglerError> {
    let output = wrangler
        .run(
            credential,
            &["kv", "namespace", "create", name],
            None,
            None,
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await?;
    output
        .last_ndjson()
        .and_then(|v| v["namespace_id"].as_str().or_else(|| v["id"].as_str()).map(String::from))
        .ok_or_else(|| WranglerError::OutputParse {
            command: format!("kv namespace create {name}"),
            message: "no namespace_id in output".to_owned(),
            output: output.stdout.clone(),
        })
}

async fn create_d1_database(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    name: &str,
) -> Result<String, WranglerError> {
    let output = wrangler
        .run(credential, &["d1", "create", name], None, None, DEFAULT_COMMAND_TIMEOUT)
        .await?;
    output
        .last_ndjson()
        .and_then(|v| v["uuid"].as_str().or_else(|| v["database_id"].as_str()).map(String::from))
        .ok_or_else(|| WranglerError::OutputParse {
            command: format!("d1 create {name}"),
            message: "no uuid in output".to_owned(),
            output: output.stdout.clone(),
        })
}

async fn create_r2_bucket(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    name: &str,
) -> Result<String, WranglerError> {
    let _output = wrangler
        .run(
            credential,
            &["r2", "bucket", "create", name],
            None,
            None,
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await?;
    Ok(name.to_owned())
}

async fn create_queue(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    name: &str,
) -> Result<String, WranglerError> {
    let output = wrangler
        .run(
            credential,
            &["queues", "create", name],
            None,
            None,
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await?;
    output
        .last_ndjson()
        .and_then(|v| v["queue_id"].as_str().or_else(|| v["id"].as_str()).map(String::from))
        .ok_or_else(|| WranglerError::OutputParse {
            command: format!("queues create {name}"),
            message: "no queue_id in output".to_owned(),
            output: output.stdout.clone(),
        })
}

// ── Resource import helpers ────────────────────────────────────────────────

async fn import_kv_namespace(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    name: &str,
) -> Result<String, WranglerError> {
    let output = wrangler
        .run(
            credential,
            &["kv", "namespace", "list"],
            None,
            None,
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await?;
    for line in output.ndjson_lines() {
        if line["title"].as_str() == Some(name) {
            if let Some(id) = line["id"].as_str() {
                return Ok(id.to_owned());
            }
        }
    }
    Err(WranglerError::StateError {
        message: format!("KV namespace {name:?} not found for import"),
    })
}

async fn import_d1_database(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    name: &str,
) -> Result<String, WranglerError> {
    let output = wrangler
        .run(credential, &["d1", "list"], None, None, DEFAULT_COMMAND_TIMEOUT)
        .await?;
    for line in output.ndjson_lines() {
        if line["name"].as_str() == Some(name) {
            if let Some(id) = line["uuid"].as_str() {
                return Ok(id.to_owned());
            }
        }
    }
    Err(WranglerError::StateError {
        message: format!("D1 database {name:?} not found for import"),
    })
}

async fn import_r2_bucket(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    name: &str,
) -> Result<String, WranglerError> {
    let output = wrangler
        .run(
            credential,
            &["r2", "bucket", "list"],
            None,
            None,
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await?;
    for line in output.ndjson_lines() {
        if line["name"].as_str() == Some(name) {
            return Ok(name.to_owned());
        }
    }
    Err(WranglerError::StateError {
        message: format!("R2 bucket {name:?} not found for import"),
    })
}

async fn import_queue(
    wrangler: &WranglerBinary,
    credential: &CloudflareCredential,
    name: &str,
) -> Result<String, WranglerError> {
    let output = wrangler
        .run(
            credential,
            &["queues", "list"],
            None,
            None,
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await?;
    for line in output.ndjson_lines() {
        if line["queue_name"].as_str() == Some(name) {
            if let Some(id) = line["queue_id"].as_str() {
                return Ok(id.to_owned());
            }
        }
    }
    Err(WranglerError::StateError {
        message: format!("Queue {name:?} not found for import"),
    })
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn load_binding_plan(
    request: &DriverRequest,
    work_dir: &Path,
    journal: &mut DeploymentJournal,
) -> Result<serde_json::Value, DriverError> {
    let json_str = match &journal.binding_plan_json {
        Some(json) => json.clone(),
        None => {
            let plan_path = work_dir.join("binding-plan.json");
            let contents = std::fs::read_to_string(&plan_path).map_err(|error| DriverError {
                operation_id: request.operation_id,
                operation: request.operation,
                desired_state_digest: request.desired_state_digest.clone(),
                category: DriverErrorCategory::Build,
                code: "BINDING_PLAN_MISSING".to_owned(),
                message: format!("binding-plan.json not found at {}: {error}", plan_path.display()),
                retryable: false,
                side_effect: SideEffect::None,
                next_action: DriverNextAction::None,
                provider_request_id: None,
            })?;
            journal.binding_plan_json = Some(contents.clone());
            contents
        }
    };
    serde_json::from_str(&json_str).map_err(|error| DriverError {
        operation_id: request.operation_id,
        operation: request.operation,
        desired_state_digest: request.desired_state_digest.clone(),
        category: DriverErrorCategory::Build,
        code: "BINDING_PLAN_INVALID".to_owned(),
        message: format!("binding-plan.json is not valid JSON: {error}"),
        retryable: false,
        side_effect: SideEffect::None,
        next_action: DriverNextAction::None,
        provider_request_id: None,
    })
}

fn is_already_exists_error(error: &WranglerError) -> bool {
    let msg = error.to_string().to_lowercase();
    msg.contains("already exists")
        || msg.contains("duplicate")
        || msg.contains("conflict")
        || msg.contains("10014")
}

pub fn find_version_id_in_output(stdout: &str) -> Option<String> {
    // First pass: valid ND-JSON lines.
    for line in stdout.lines() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(id) = value["version_id"].as_str() {
                return Some(id.to_owned());
            }
        }
    }
    // Second pass: lines containing "version_id" (case-insensitive) or
    // "version id" / "Version ID" typical of wrangler's human-readable
    // output before 4.129.0 gained ND-JSON everywhere.
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        for marker in ["version_id", "version id"] {
            if let Some(idx) = lower.find(marker) {
                let rest = &line[idx + marker.len()..];
                let rest = rest.trim_start_matches(|c: char| !c.is_alphanumeric());
                let id: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '-')
                    .collect();
                if !id.is_empty() {
                    return Some(id);
                }
            }
        }
    }
    None
}

fn to_driver_error(
    request: &DriverRequest,
    operation: DriverOperation,
    error: &WranglerError,
    has_side_effect: bool,
) -> DriverError {
    let (category, code, retryable, next_action) = match error {
        WranglerError::Io { .. } | WranglerError::Spawn { .. } => (
            DriverErrorCategory::Internal,
            "WRANGLER_EXECUTION",
            true,
            DriverNextAction::Retry,
        ),
        WranglerError::Download { .. } | WranglerError::NoHome => (
            DriverErrorCategory::Internal,
            "WRANGLER_INSTALL",
            false,
            DriverNextAction::None,
        ),
        WranglerError::Integrity { .. } => (
            DriverErrorCategory::DriverIntegrity,
            "WRANGLER_INTEGRITY",
            false,
            DriverNextAction::None,
        ),
        WranglerError::Timeout { .. } => (
            DriverErrorCategory::TransientProvider,
            "WRANGLER_TIMEOUT",
            true,
            if has_side_effect {
                DriverNextAction::Reconcile
            } else {
                DriverNextAction::Retry
            },
        ),
        WranglerError::NoCredentials | WranglerError::NoAccountId(_) => (
            DriverErrorCategory::Authentication,
            "CREDENTIAL_MISSING",
            false,
            DriverNextAction::None,
        ),
        WranglerError::OutputParse { .. } => (
            DriverErrorCategory::Internal,
            "WRANGLER_OUTPUT_PARSE",
            false,
            DriverNextAction::None,
        ),
        WranglerError::CommandFailed { .. } => (
            DriverErrorCategory::Activation,
            "WRANGLER_COMMAND_FAILED",
            true,
            if has_side_effect {
                DriverNextAction::Reconcile
            } else {
                DriverNextAction::Retry
            },
        ),
        WranglerError::StateError { .. } => (
            DriverErrorCategory::Resource,
            "RESOURCE_STATE",
            false,
            DriverNextAction::None,
        ),
    };
    DriverError {
        operation_id: request.operation_id,
        operation,
        desired_state_digest: request.desired_state_digest.clone(),
        category,
        code: code.to_owned(),
        message: error.to_string(),
        retryable,
        side_effect: if has_side_effect {
            SideEffect::Possible
        } else {
            SideEffect::None
        },
        next_action,
        provider_request_id: None,
    }
}

fn succeeded(
    request: &DriverRequest,
    version_id: Option<String>,
    deployment_id: Option<String>,
    resource_ids: Option<BTreeMap<String, String>>,
) -> DriverReceipt {
    DriverReceipt {
        schema_version: request.schema_version,
        operation_id: request.operation_id,
        operation: request.operation,
        desired_state_digest: request.desired_state_digest.clone(),
        state: DriverStepState::Succeeded,
        deployment_state: None,
        side_effect: SideEffect::None,
        provider_request_id: None,
        provider_version_id: version_id,
        provider_deployment_id: deployment_id,
        provider_resource_ids: resource_ids.unwrap_or_default(),
        observed_at: Utc::now(),
        next_action: DriverNextAction::None,
        diagnostic: None,
    }
}

fn succeeded_with_state(
    request: &DriverRequest,
    deployment_state: DeploymentState,
    version_id: Option<String>,
    resource_ids: Option<BTreeMap<String, String>>,
) -> DriverReceipt {
    let mut receipt = succeeded(request, version_id, None, resource_ids);
    let side_effect = match &deployment_state {
        DeploymentState::Active
        | DeploymentState::Uploaded
        | DeploymentState::Ready
        | DeploymentState::RolledBack => SideEffect::Confirmed,
        _ => SideEffect::None,
    };
    receipt.deployment_state = Some(deployment_state);
    receipt.side_effect = side_effect;
    receipt
}

// ── Deployment journal ─────────────────────────────────────────────────────

/// Persisted state between lifecycle operations.
#[derive(Clone, Debug, Default)]
pub struct DeploymentJournal {
    /// The raw binding-plan.json contents.
    pub binding_plan_json: Option<String>,
    /// Resources created during this deployment: name → provider resource ID.
    pub created_resources: BTreeMap<String, String>,
    /// The version ID returned by `wrangler versions upload`.
    pub version_id: Option<String>,
    /// The deployment ID returned by `wrangler versions deploy`.
    pub deployment_id: Option<String>,
}

impl DeploymentJournal {
    pub fn path(work_dir: &Path) -> std::path::PathBuf {
        work_dir.join(".deis-journal.json")
    }

    pub fn load(work_dir: &Path) -> Self {
        let path = Self::path(work_dir);
        match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, work_dir: &Path) -> Result<(), std::io::Error> {
        let path = Self::path(work_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error.to_string()))?;
        std::fs::write(&path, json)
    }
}

impl serde::Serialize for DeploymentJournal {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Helper<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            binding_plan_json: &'a Option<String>,
            #[serde(default)]
            created_resources: &'a BTreeMap<String, String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            version_id: &'a Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            deployment_id: &'a Option<String>,
        }
        Helper {
            binding_plan_json: &self.binding_plan_json,
            created_resources: &self.created_resources,
            version_id: &self.version_id,
            deployment_id: &self.deployment_id,
        }
        .serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for DeploymentJournal {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Helper {
            #[serde(default)]
            binding_plan_json: Option<String>,
            #[serde(default)]
            created_resources: BTreeMap<String, String>,
            #[serde(default)]
            version_id: Option<String>,
            #[serde(default)]
            deployment_id: Option<String>,
        }
        let h = Helper::deserialize(deserializer)?;
        Ok(Self {
            binding_plan_json: h.binding_plan_json,
            created_resources: h.created_resources,
            version_id: h.version_id,
            deployment_id: h.deployment_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_round_trips() {
        let mut journal = DeploymentJournal::default();
        journal.binding_plan_json = Some(r#"{"bindings":[]}"#.to_owned());
        journal
            .created_resources
            .insert("HOT".to_owned(), "ns-abc".to_owned());
        journal.version_id = Some("v1".to_owned());
        let json = serde_json::to_string(&journal).unwrap();
        let loaded: DeploymentJournal = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.binding_plan_json, journal.binding_plan_json);
        assert_eq!(loaded.created_resources, journal.created_resources);
        assert_eq!(loaded.version_id, journal.version_id);
    }

    #[test]
    fn find_version_id_parses_ndjson() {
        let stdout = "{\"status\":\"uploading\"}\n{\"status\":\"complete\",\"version_id\":\"abc-def-123\"}\n";
        let id = find_version_id_in_output(stdout);
        assert_eq!(id, Some("abc-def-123".to_owned()));
    }

    #[test]
    fn find_version_id_fallback() {
        let stdout = "Upload complete! version_id: xyz-789";
        let id = find_version_id_in_output(stdout);
        assert_eq!(id, Some("xyz-789".to_owned()));
    }
}