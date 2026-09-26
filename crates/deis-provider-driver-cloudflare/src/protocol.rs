//! Protocol-level entry: JCS JSON on stdin → single operation → JCS JSON on stdout.
//!
//! The binary reads a [`deis_provider_driver::DriverProcessRequest`] envelope
//! from stdin (one operation per invocation), dispatches it to wrangler,
//! and writes back a [`deis_provider_driver::DriverProcessOutcome`].

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use deis_provider_driver::{
    DriverError, DriverErrorCategory, DriverNextAction, DriverProcessOutcome,
    DriverProcessRequest, SideEffect,
};
use deis_wire::CanonicalId;
use tracing::{error, info, warn};

use crate::commands::DeploymentJournal;
use crate::credentials::CloudflareCredential;
use crate::wrangler::WranglerBinary;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(300);

/// A zero digest for placeholder errors before real request data is available.
fn zero_digest() -> deis_provider_driver::Sha256Digest {
    "0000000000000000000000000000000000000000000000000000000000000000"
        .parse()
        .unwrap()
}

/// Read request from stdin, execute, write outcome to stdout.
pub async fn execute_from_stdin() -> Result<(), Box<dyn std::error::Error>> {
    let stdin_data = io::read_to_string(io::stdin())?;
    if stdin_data.trim().is_empty() {
        let outcome = DriverProcessOutcome::failed(DriverError {
            operation_id: CanonicalId::new_v7(),
            operation: deis_provider_driver::DriverOperation::Preflight,
            desired_state_digest: zero_digest(),
            category: DriverErrorCategory::DriverIntegrity,
            code: "EMPTY_STDIN".to_owned(),
            message: "stdin is empty, expected JCS-encoded DriverProcessRequest".to_owned(),
            retryable: false,
            side_effect: SideEffect::None,
            next_action: DriverNextAction::None,
            provider_request_id: None,
        });
        print_outcome(&outcome)?;
        return Ok(());
    }

    let process_request: DriverProcessRequest = match serde_json::from_str(&stdin_data) {
        Ok(r) => r,
        Err(error) => {
            let outcome = DriverProcessOutcome::failed(DriverError {
                operation_id: CanonicalId::new_v7(),
                operation: deis_provider_driver::DriverOperation::Preflight,
                desired_state_digest: zero_digest(),
                category: DriverErrorCategory::DriverIntegrity,
                code: "PARSE_ERROR".to_owned(),
                message: format!("failed to parse DriverProcessRequest: {error}"),
                retryable: false,
                side_effect: SideEffect::None,
                next_action: DriverNextAction::None,
                provider_request_id: None,
            });
            print_outcome(&outcome)?;
            return Ok(());
        }
    };

    let request = match process_request.validate() {
        Ok(r) => r,
        Err(_) => {
            let outcome = DriverProcessOutcome::failed(DriverError {
                operation_id: CanonicalId::new_v7(),
                operation: deis_provider_driver::DriverOperation::Preflight,
                desired_state_digest: zero_digest(),
                category: DriverErrorCategory::DriverIntegrity,
                code: "PROTOCOL_MISMATCH".to_owned(),
                message: "driver process request does not use the exact v1 protocol".to_owned(),
                retryable: false,
                side_effect: SideEffect::None,
                next_action: DriverNextAction::None,
                provider_request_id: None,
            });
            print_outcome(&outcome)?;
            return Ok(());
        }
    };

    match tokio::time::timeout(PROCESS_TIMEOUT, execute_request(request)).await {
        Ok(result) => result,
        Err(_) => {
            let outcome = DriverProcessOutcome::failed(DriverError {
                operation_id: CanonicalId::new_v7(),
                operation: deis_provider_driver::DriverOperation::Preflight,
                desired_state_digest: zero_digest(),
                category: DriverErrorCategory::Internal,
                code: "DRIVER_TIMEOUT".to_owned(),
                message: format!("driver process timed out after {PROCESS_TIMEOUT:?}"),
                retryable: true,
                side_effect: SideEffect::Possible,
                next_action: DriverNextAction::Reconcile,
                provider_request_id: None,
            });
            print_outcome(&outcome)?;
            Ok(())
        }
    }
}

async fn execute_request(
    request: deis_provider_driver::DriverRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let credential = match load_credential(&request) {
        Ok(cred) => cred,
        Err(outcome) => {
            print_outcome(&outcome)?;
            return Ok(());
        }
    };

    let wrangler = match WranglerBinary::ensure_installed().await {
        Ok(w) => w,
        Err(error) => {
            error!(%error, "Failed to install wrangler");
            let outcome = DriverProcessOutcome::failed(DriverError {
                operation_id: request.operation_id,
                operation: request.operation,
                desired_state_digest: request.desired_state_digest.clone(),
                category: DriverErrorCategory::Internal,
                code: "WRANGLER_INSTALL_FAILED".to_owned(),
                message: error.to_string(),
                retryable: true,
                side_effect: SideEffect::None,
                next_action: DriverNextAction::Retry,
                provider_request_id: None,
            });
            print_outcome(&outcome)?;
            return Ok(());
        }
    };

    let work_dir = determine_work_dir();
    let mut journal = DeploymentJournal::load(&work_dir);

    info!(
        operation_id = %request.operation_id,
        operation = ?request.operation,
        "Executing operation"
    );

    let outcome = match crate::commands::execute_operation(
        &wrangler,
        &credential,
        &request,
        &work_dir,
        &mut journal,
    )
    .await
    {
        Ok(receipt) => {
            info!(
                operation_id = %request.operation_id,
                state = ?receipt.state,
                "Operation completed"
            );
            // Save journal for crash resilience
            let mut journal_for_save = journal.clone();
            if let Some(ref vid) = receipt.provider_version_id {
                journal_for_save.version_id = Some(vid.clone());
            }
            if let Some(ref did) = receipt.provider_deployment_id {
                journal_for_save.deployment_id = Some(did.clone());
            }
            if let Err(error) = journal_for_save.save(&work_dir) {
                warn!(%error, "Failed to save deployment journal");
            }
            DriverProcessOutcome::succeeded(receipt)
        }
        Err(error) => {
            error!(
                operation_id = %request.operation_id,
                ?error,
                "Operation failed"
            );
            DriverProcessOutcome::failed(error)
        }
    };

    print_outcome(&outcome)?;
    Ok(())
}

fn load_credential(
    request: &deis_provider_driver::DriverRequest,
) -> Result<CloudflareCredential, DriverProcessOutcome> {
    let value = match std::env::var("EDGE_CANON_PROVIDER_CREDENTIAL") {
        Ok(v) => v,
        Err(_) => {
            return Err(DriverProcessOutcome::failed(DriverError {
                operation_id: request.operation_id,
                operation: request.operation,
                desired_state_digest: request.desired_state_digest.clone(),
                category: DriverErrorCategory::Authentication,
                code: "CREDENTIAL_NOT_SET".to_owned(),
                message: "EDGE_CANON_PROVIDER_CREDENTIAL is not set".to_owned(),
                retryable: false,
                side_effect: SideEffect::None,
                next_action: DriverNextAction::None,
                provider_request_id: None,
            }));
        }
    };
    CloudflareCredential::from_env_value(&value).map_err(|error| {
        DriverProcessOutcome::failed(DriverError {
            operation_id: request.operation_id,
            operation: request.operation,
            desired_state_digest: request.desired_state_digest.clone(),
            category: DriverErrorCategory::Authentication,
            code: "CREDENTIAL_PARSE_FAILED".to_owned(),
            message: error.to_string(),
            retryable: false,
            side_effect: SideEffect::None,
            next_action: DriverNextAction::None,
            provider_request_id: None,
        })
    })
}

fn determine_work_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn print_outcome(outcome: &DriverProcessOutcome) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_jcs::to_string(outcome)?;
    println!("{json}");
    Ok(())
}