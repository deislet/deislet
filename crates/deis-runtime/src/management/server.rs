use std::sync::Arc;

use crate::app::ApplicationManager;
use crate::app_repo::AppRepo;
use crate::config_store::ConfigStore;

use super::proto::runtime_management_server::RuntimeManagementServer as ProtoServer;
use super::service::RuntimeManagementService;

pub struct ManagementServer;

impl ManagementServer {
    /// Create gRPC service routing (for embedding into axum)
    pub fn create_service(
        app_manager: Arc<ApplicationManager>,
        config_store: Arc<ConfigStore>,
        app_repo: Arc<AppRepo>,
        node_id: String,
    ) -> ProtoServer<RuntimeManagementService> {
        let service = RuntimeManagementService::new(app_manager, config_store, app_repo, node_id);
        ProtoServer::new(service)
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_SIZE)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_SIZE)
    }
}
