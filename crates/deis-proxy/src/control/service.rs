//! Runs the control client as a pingora background service.

use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::sync::Mutex;
use tracing::{error, info};

use super::ProxyControlClient;

pub struct ControlService {
    client: Mutex<ProxyControlClient>,
}

impl ControlService {
    pub fn new(client: ProxyControlClient) -> Self {
        Self {
            client: Mutex::new(client),
        }
    }
}

#[async_trait]
impl BackgroundService for ControlService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let mut client = self.client.lock().await;

        // The client retries the connection until shutdown, so racing it
        // against the shutdown watch here is what cancels a pending retry
        // delay promptly.
        let stopped = tokio::select! {
            _ = client.run() => true,
            _ = shutdown.changed() => false,
        };

        if stopped {
            error!("control client stopped unexpectedly; instance is no longer managed");
            return;
        }
        info!("server shutting down; disconnecting from control");
        if let Err(e) = client.disconnect().await {
            error!(error = %e, "control disconnect failed");
        }
    }
}
