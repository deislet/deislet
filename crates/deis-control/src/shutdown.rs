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

#[cfg(unix)]
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
#[cfg(unix)]
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

#[cfg(not(unix))]
pub async fn signal() {
    report_interrupt(tokio::signal::ctrl_c().await);
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

/// A stop signal several parts of the process can wait on.
///
/// [`signal`] resolves once, in one place. Everything that has to react to a
/// stop needs the same edge: the HTTP/gRPC server starts draining, the cron
/// scheduler stops firing, the node health poller stops probing, and every
/// open `EstablishChannel` stream is closed so the drain does not sit out its
/// whole grace period waiting for streams that end only when a node goes away.
///
/// Cloning gives another view of the same signal; triggering it once wakes
/// every waiter, and a waiter that arrives after the trigger returns
/// immediately rather than waiting for a second edge that never comes.
#[derive(Clone)]
pub struct Shutdown {
    /// `Arc` because `watch::Sender` is not itself clonable, and every holder
    /// must be able to trigger as well as observe - the signal listener does
    /// one, the servers do the other.
    state: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
}

impl Shutdown {
    pub fn new() -> Self {
        let (state, _) = tokio::sync::watch::channel(false);
        Self {
            state: std::sync::Arc::new(state),
        }
    }

    /// Announce that the process is stopping. Idempotent.
    pub fn trigger(&self) {
        // `send_replace` rather than `send`, and the difference is not
        // cosmetic: `send` refuses when no receiver exists at that instant and
        // leaves the value untouched, so a stop announced before the loops had
        // subscribed - or between two of the poller's `select` arms, which
        // subscribes and drops a receiver per tick - would be dropped on the
        // floor. Nothing would ever see it, and the process would keep firing
        // cron and probing nodes until it was killed. `send_replace` always
        // stores the value, so a waiter arriving afterwards still sees it.
        self.state.send_replace(true);
    }

    /// Whether the stop has already been announced.
    ///
    /// Used by loops that must not begin another unit of work - a cron fire, a
    /// health probe - after the signal, without having to be at an await point.
    pub fn is_triggered(&self) -> bool {
        *self.state.borrow()
    }

    /// Resolve when the stop is announced, immediately if it already was.
    pub async fn wait(&self) {
        let mut rx = self.state.subscribe();
        loop {
            let triggered = *rx.borrow_and_update();
            if triggered {
                return;
            }
            // The sender lives in this same struct, so `changed` can only
            // fail if every sender was dropped, which means nothing will
            // ever trigger: treat it as "stop waiting" rather than hang.
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`Shutdown`] wired to the process's stop signals.
///
/// The listener runs in its own task so the caller can hand clones of the
/// handle to the servers and the background loops before any of them start.
pub fn on_signal() -> Shutdown {
    let shutdown = Shutdown::new();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            signal().await;
            shutdown.trigger();
        }
    });
    shutdown
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A waiter that arrives after the trigger must not wait for a second
    /// edge: the background loops subscribe at different moments, and one
    /// that subscribed late would otherwise keep firing at nodes this
    /// process is walking away from.
    #[tokio::test]
    async fn a_late_waiter_sees_a_stop_that_already_happened() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.is_triggered());
        shutdown.trigger();
        assert!(shutdown.is_triggered());
        shutdown.wait().await;
    }

    #[tokio::test]
    async fn every_clone_is_woken_by_one_trigger() {
        let shutdown = Shutdown::new();
        let waiters: Vec<_> = (0..3)
            .map(|_| {
                let shutdown = shutdown.clone();
                tokio::spawn(async move { shutdown.wait().await })
            })
            .collect();
        shutdown.trigger();
        for waiter in waiters {
            waiter.await.unwrap();
        }
    }
}
