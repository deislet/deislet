use super::proto::trigger_server::Trigger;
use super::proto::*;
use crate::app::ApplicationManager;
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::info;

pub struct TriggerServiceImpl {
    #[allow(dead_code)] // Will be used when executing logic is implemented
    app_manager: Arc<ApplicationManager>,
}

impl TriggerServiceImpl {
    pub fn new(app_manager: Arc<ApplicationManager>) -> Self {
        Self { app_manager }
    }
}

#[tonic::async_trait]
impl Trigger for TriggerServiceImpl {
    async fn on_queue(
        &self,
        request: Request<QueueEvent>,
    ) -> Result<Response<TriggerResponse>, Status> {
        let req = request.into_inner();
        info!("Queue trigger: app={}, queue={}", req.app_id, req.queue_name);
        
        // TODO: Call app_manager.execute_queue(...)
        
        Ok(Response::new(TriggerResponse {
            success: true,
            failed_message_ids: vec![],
            error: String::new(),
        }))
    }

    async fn on_scheduled(
        &self,
        request: Request<ScheduledEvent>,
    ) -> Result<Response<TriggerResponse>, Status> {
        let req = request.into_inner();
        info!("Scheduled trigger: app={}, cron={}", req.app_id, req.cron);
        
        // TODO: Call app_manager.execute_scheduled(...)
        
        Ok(Response::new(TriggerResponse {
            success: true,
            failed_message_ids: vec![],
            error: String::new(),
        }))
    }
}
