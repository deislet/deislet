use super::service::TriggerServiceImpl;
use crate::app::ApplicationManager;
use std::sync::Arc;

pub struct TriggerServer;

impl TriggerServer {
    pub fn create_service(
        app_manager: Arc<ApplicationManager>,
    ) -> super::proto::trigger_server::TriggerServer<TriggerServiceImpl> {
        let service = TriggerServiceImpl::new(app_manager);
        super::proto::trigger_server::TriggerServer::new(service)
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
    }
}
