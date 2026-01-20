use std::sync::Arc;

use crate::app::ApplicationManager;
use crate::app_store::AppStore;
use crate::config_store::ConfigStore;

use super::proto::denix_management_server::DenixManagementServer as ProtoServer;
use super::service::DenixManagementService;

pub struct ManagementServer;

impl ManagementServer {
    /// 创建 gRPC 服务路由（用于嵌入到 axum）
    pub fn create_service(
        app_manager: Arc<ApplicationManager>,
        config_store: Arc<ConfigStore>,
        app_store: Arc<AppStore>,
        worker_id: String,
    ) -> ProtoServer<DenixManagementService> {
        let service = DenixManagementService::new(app_manager, config_store, app_store, worker_id);
        ProtoServer::new(service)
    }
}
