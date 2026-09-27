//! Protocol-level entry: JCS JSON on stdin → single operation → JCS JSON on stdout.
//!
//! The binary reads a [`deis_provider_driver::DriverProcessRequest`] envelope
//! from stdin (one operation per invocation), dispatches it to wrangler,
//! and writes back a [`deis_provider_driver::DriverProcessOutcome`].

use std::io;
use std::time::Duration;

use deis_provider_driver::{
    DriverError, DriverErrorCategory, DriverNextAction, DriverProcessOutcome, DriverProcessRequest,
    SideEffect,
};
use deis_wire::CanonicalId;
use tracing::{error, info};

use crate::commands::{self, OperationContext, Timeouts};
use crate::credentials::CloudflareCredential;
use crate::environment::DriverEnvironment;
use crate::journal::JournalStore;
use crate::toolchain::{
    EMBEDDED_TOOLCHAIN_LOCK, HttpFetcher, Installer, NodeRuntime, Platform, ToolchainPin,
};
use crate::wrangler::{Wrangler, WranglerError};

/// The whole process's budget, including a first-run toolchain install.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(600);

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
    let outcome = run_request(&request).await;
    print_outcome(&outcome)?;
    Ok(())
}

/// Resolve the driver's inputs, install/verify the toolchain, and run one
/// operation.
async fn run_request(request: &deis_provider_driver::DriverRequest) -> DriverProcessOutcome {
    let credential = match load_credential(request) {
        Ok(credential) => credential,
        Err(outcome) => return *outcome,
    };
    let environment = match DriverEnvironment::from_process() {
        Ok(environment) => environment,
        Err(message) => {
            return setup_failure(
                request,
                DriverErrorCategory::Configuration,
                "DRIVER_ENVIRONMENT",
                message,
                false,
            );
        }
    };

    let wrangler = match install_wrangler(&environment).await {
        Ok(wrangler) => wrangler,
        Err(error) => {
            error!(%error, "Failed to prepare wrangler");
            let category = match error {
                WranglerError::Integrity { .. } => DriverErrorCategory::DriverIntegrity,
                _ => DriverErrorCategory::Internal,
            };
            let retryable = !matches!(
                error,
                WranglerError::Integrity { .. } | WranglerError::Node { .. }
            );
            return setup_failure(
                request,
                category,
                "WRANGLER_INSTALL_FAILED",
                error.to_string(),
                retryable,
            );
        }
    };

    let journals = JournalStore::new(&environment.state_dir);
    let scratch_root = environment.state_dir.join("cloudflare").join("work");
    let context = OperationContext {
        wrangler: &wrangler,
        credential: &credential,
        artifact_dir: environment.artifact_dir(),
        journals: &journals,
        scratch_root: &scratch_root,
        config: &environment.config,
        secrets: &environment.secrets,
        purge_environment_id: environment.purge_environment_id.as_deref(),
        timeouts: Timeouts::default(),
    };

    info!(
        operation_id = %request.operation_id,
        operation = ?request.operation,
        "Executing operation"
    );
    match commands::execute_operation(&context, request).await {
        Ok(receipt) => {
            info!(operation_id = %request.operation_id, state = ?receipt.state, "Operation completed");
            DriverProcessOutcome::succeeded(receipt)
        }
        Err(error) => {
            error!(operation_id = %request.operation_id, code = %error.code, "Operation failed");
            DriverProcessOutcome::failed(error)
        }
    }
}

async fn install_wrangler(environment: &DriverEnvironment) -> Result<Wrangler, WranglerError> {
    let node = NodeRuntime::locate(
        environment.node.as_deref(),
        environment.search_path.as_deref(),
        crate::MINIMUM_NODE_MAJOR,
    )
    .await?;
    let fetcher = HttpFetcher::new()?;
    let pin = ToolchainPin::wrangler();
    let toolchain = Installer {
        tools_root: &environment.tools_dir,
        lock_json: EMBEDDED_TOOLCHAIN_LOCK,
        pin: &pin,
        fetcher: &fetcher,
        platform: Platform::host(),
    }
    .ensure()
    .await?;
    Ok(Wrangler::installed(
        &node,
        &toolchain,
        environment.wrangler_home(),
    ))
}

fn setup_failure(
    request: &deis_provider_driver::DriverRequest,
    category: DriverErrorCategory,
    code: &str,
    message: String,
    retryable: bool,
) -> DriverProcessOutcome {
    DriverProcessOutcome::failed(DriverError {
        operation_id: request.operation_id,
        operation: request.operation,
        desired_state_digest: request.desired_state_digest.clone(),
        category,
        code: code.to_owned(),
        message,
        retryable,
        side_effect: SideEffect::None,
        next_action: if retryable {
            DriverNextAction::Retry
        } else {
            DriverNextAction::None
        },
        provider_request_id: None,
    })
}

fn load_credential(
    request: &deis_provider_driver::DriverRequest,
) -> Result<CloudflareCredential, Box<DriverProcessOutcome>> {
    let failure = |code: &str, message: String| {
        Box::new(setup_failure(
            request,
            DriverErrorCategory::Authentication,
            code,
            message,
            false,
        ))
    };
    let mut value = std::env::var("EDGE_CANON_PROVIDER_CREDENTIAL").map_err(|_| {
        failure(
            "CREDENTIAL_NOT_SET",
            "EDGE_CANON_PROVIDER_CREDENTIAL is not set".to_owned(),
        )
    })?;
    let credential = CloudflareCredential::from_env_value(&value)
        .map_err(|error| failure("CREDENTIAL_PARSE_FAILED", error.to_string()));
    zeroize::Zeroize::zeroize(&mut value);
    credential
}

fn print_outcome(outcome: &DriverProcessOutcome) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_jcs::to_string(outcome)?;
    println!("{json}");
    Ok(())
}
