//! Makes a listener failure fatal.
//!
//! Pingora builds a service's listeners inside `start_service` and panics
//! there when a bind fails (`EACCES` on a privileged port after the identity
//! switch, `EADDRNOTAVAIL` for a wrong `listen_addr`, ...). The panic only
//! kills the service task: the server's main loop keeps running, the PID
//! file stays written, and in daemon mode nothing is listening and nothing
//! is logged. Wrapping the service turns that into a logged exit so the
//! failure is visible to the operator and to the service manager.

use std::panic::AssertUnwindSafe;

use async_trait::async_trait;
use futures::FutureExt;
use pingora::server::{ListenFds, ShutdownWatch};
use pingora::services::Service;
use tracing::error;

/// Runs the wrapped service and terminates the process if it panics.
pub struct ExitOnPanic<S> {
    inner: S,
}

impl<S: Service> ExitOnPanic<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl<S: Service> Service for ExitOnPanic<S> {
    async fn start_service(
        &mut self,
        fds: Option<ListenFds>,
        shutdown: ShutdownWatch,
        listeners_per_fd: usize,
    ) {
        let name = self.inner.name().to_string();
        let run = self.inner.start_service(fds, shutdown, listeners_per_fd);
        if let Err(payload) = AssertUnwindSafe(run).catch_unwind().await {
            let reason = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            error!(
                service = %name,
                reason,
                "service failed to start or crashed; exiting so the failure is not silent"
            );
            std::process::exit(1);
        }
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn threads(&self) -> Option<usize> {
        self.inner.threads()
    }
}
