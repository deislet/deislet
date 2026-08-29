use std::sync::Arc;
use std::time::Instant;

use crate::app::ApplicationManager;
use crate::deployment::DeploymentManager;
use crate::health::NodeHealth;

use super::proto::runtime_management_server::RuntimeManagementServer as ProtoServer;
use super::service::RuntimeManagementService;

pub struct ManagementServer;

impl ManagementServer {
    /// Create gRPC service routing (for embedding into axum)
    pub fn create_service(
        app_manager: Arc<ApplicationManager>,
        deployer: Arc<DeploymentManager>,
        health: Arc<NodeHealth>,
        node_id: String,
        // When this node's process started, for the uptime `GetStatus`
        // reports.
        started_at: Instant,
    ) -> ProtoServer<RuntimeManagementService> {
        let service =
            RuntimeManagementService::new(app_manager, deployer, health, node_id, started_at);
        ProtoServer::new(service)
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
    }
}
