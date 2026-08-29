//! Stopping the control plane on purpose.
//!
//! Until this existed a SIGTERM killed the process mid-request: an upload
//! being written to the bundle directory, a deployment pointer being
//! inserted, a node halfway through fetching its assignments. The control
//! plane is restarted by the same supervisors as every other component, so
//! it now listens for the same two signals deis-store and deis-telemetry
//! listen for and hands the servers a bounded window to finish what they
//! are holding.

use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal as unix_signal};
use tracing::{error, info};

/// How long in-flight work gets after the stop signal before it is cut.
///
/// A deadline is required rather than optional here: every connected node
/// holds an `EstablishChannel` gRPC stream open for its whole lifetime, and
/// those streams end only when the node goes away. Waiting for them would
/// mean never stopping, and the supervisor would eventually SIGKILL the
/// process - losing exactly the in-flight requests draining is meant to
/// protect. Fifteen seconds is well above the slowest request the control
/// plane serves (a bundle upload writing to disk) and well below the ten
/// second to one minute a supervisor typically allows before SIGKILL.
pub const DRAIN_GRACE: Duration = Duration::from_secs(15);

/// Resolve when the process is asked to stop.
///
/// Both signals are honoured, because both are used: an operator pressing
/// Ctrl-C sends SIGINT, while systemd, Docker and Kubernetes stop a
/// container with SIGTERM. Listening for SIGINT alone would leave this
/// process killed outright by every supervisor there is, skipping the
/// drain entirely.
pub async fn signal() {
    let mut terminate = match unix_signal(SignalKind::terminate()) {
        Ok(stream) => Some(stream),
        Err(e) => {
            error!(
                error = %e,
                "Cannot listen for SIGTERM; a supervisor stopping the control plane will kill it \
                 outright, cutting in-flight uploads and deployment writes"
            );
            None
        }
    };

    match terminate.as_mut() {
        Some(terminate) => {
            tokio::select! {
                result = tokio::signal::ctrl_c() => report_interrupt(result),
                _ = terminate.recv() => info!("SIGTERM received, starting shutdown"),
            }
        }
        None => report_interrupt(tokio::signal::ctrl_c().await),
    }
}

fn report_interrupt(result: std::io::Result<()>) {
    match result {
        Ok(()) => info!("Interrupt received, starting shutdown"),
        Err(e) => error!(
            error = %e,
            "Cannot listen for the interrupt signal; the process will only stop when killed, \
             which drops in-flight requests"
        ),
    }
}
