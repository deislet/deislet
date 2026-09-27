//! Binary entrypoint for the Cloudflare provider driver.
//!
//! ## Protocol
//!
//! Reads a JCS-encoded
//! [`deis_provider_driver::DriverProcessRequest`] from stdin and writes a
//! JCS-encoded [`deis_provider_driver::DriverProcessOutcome`] to stdout.
//!
//! ## Usage
//!
//! ```text
//! EDGE_CANON_PROVIDER_CREDENTIAL='{"api_token":"...", "account_id":"..."}' \
//!   deis-provider-driver-cloudflare < driver-process-request.jcs.json
//! ```
//!
//! Or with the driver framework:
//!
//! ```text
//! deis-provider-driver-cloudflare execute --protocol edge-canon-provider-driver-v1
//! ```

use std::process;

use deis_provider_driver::{
    DriverError, DriverErrorCategory, DriverNextAction, DriverProcessOutcome, SideEffect,
};
use deis_wire::CanonicalId;

fn zero_digest() -> deis_provider_driver::Sha256Digest {
    "0000000000000000000000000000000000000000000000000000000000000000"
        .parse()
        .unwrap()
}

#[tokio::main]
async fn main() {
    // Initialise logging to stderr (so stdout stays clean for protocol output).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    if let Err(error) = deis_provider_driver_cloudflare::protocol::execute_from_stdin().await {
        tracing::error!(%error, "Driver process failed");
        // If we haven't written a protocol output yet, write a Failed.
        let outcome = DriverProcessOutcome::failed(DriverError {
            operation_id: CanonicalId::new_v7(),
            operation: deis_provider_driver::DriverOperation::Preflight,
            desired_state_digest: zero_digest(),
            category: DriverErrorCategory::Internal,
            code: "DRIVER_PANIC".to_owned(),
            message: error.to_string(),
            retryable: true,
            side_effect: SideEffect::Possible,
            next_action: DriverNextAction::Reconcile,
            provider_request_id: None,
        });
        if let Ok(json) = serde_jcs::to_string(&outcome) {
            println!("{json}");
        }
        process::exit(1);
    }
}
