//! Runs the supervisor client as a pingora background service.

use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::sync::Mutex;
use tracing::{error, info};

use super::DefluxSupervisorClient;

pub struct SupervisorService {
    client: Mutex<DefluxSupervisorClient>,
}

impl SupervisorService {
    pub fn new(client: DefluxSupervisorClient) -> Self {
        Self {
            client: Mutex::new(client),
        }
    }
}

#[async_trait]
impl BackgroundService for SupervisorService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let mut client = self.client.lock().await;

        let outcome = tokio::select! {
            result = async {
                client.start().await?;
                info!("supervisor client connected");
                client.run().await
            } => Some(result),
            _ = shutdown.changed() => None,
        };

        match outcome {
            Some(Ok(())) => info!("supervisor client stopped"),
            Some(Err(e)) => {
                error!(error = %e, "supervisor client failed; instance is no longer managed")
            }
            None => {
                info!("server shutting down; disconnecting from supervisor");
                if let Err(e) = client.disconnect().await {
                    error!(error = %e, "supervisor disconnect failed");
                }
            }
        }
    }
}
