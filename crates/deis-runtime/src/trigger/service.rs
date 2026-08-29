//! The trigger service: work that arrives without a request.
//!
//! Two things can start an execution that no HTTP client asked for. Both are
//! pushed at this node by whoever owns the schedule or the message, and both
//! answer honestly whether the work actually ran, because the caller's next
//! move depends on it:
//!
//! * **`OnScheduled`** - the control plane owns cron. A cron expression
//!   belongs to an application, not to a node (architecture principle 6), so
//!   the control plane keeps the schedule, picks one healthy node per tick and
//!   calls this. A failure here means it should try another node rather than
//!   count the tick as done. A tick is never made up later.
//! * **`OnQueue`** - a batch pushed in from outside, rather than leased by
//!   this node's own poller. The response names the messages the handler did
//!   not complete, and the caller owns acknowledging the rest: this node holds
//!   no lease it could act on.

use super::proto::trigger_server::Trigger;
use super::proto::*;
use crate::app::ApplicationManager;
use crate::runtime::LeasedMessage;
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

pub struct TriggerServiceImpl {
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
        let all_ids: Vec<String> = req.messages.iter().map(|m| m.id.clone()).collect();

        if req.messages.is_empty() {
            // Nothing to do, and nothing failed. Saying so is cheaper than
            // starting an isolate to hand it an empty batch.
            return Ok(Response::new(TriggerResponse {
                success: true,
                failed_message_ids: Vec::new(),
                error: String::new(),
            }));
        }

        info!(
            app_id = %req.app_id,
            queue = %req.queue_name,
            messages = req.messages.len(),
            "Queue batch pushed at this node"
        );

        let messages: Vec<LeasedMessage> = req
            .messages
            .into_iter()
            .map(|message| LeasedMessage {
                id: message.id,
                body: message.body,
                attempts: message.attempts,
                // This node never leased these messages, so it has no lease to
                // report and nothing it could ack or nack. Whoever pushed them
                // holds that and reads the answer below.
                lease_expires_at: 0,
                created_at: message.timestamp,
            })
            .collect();

        match self
            .app_manager
            .execute_queue_batch(&req.app_id, req.queue_name.clone(), messages)
            .await
        {
            Ok(outcome) => {
                if !outcome.failed.is_empty() {
                    warn!(
                        app_id = %req.app_id,
                        queue = %req.queue_name,
                        failed = outcome.failed.len(),
                        "The handler did not complete every message in the pushed batch; the \
                         listed ids are handed back to whoever pushed them and must be \
                         redelivered or they are lost"
                    );
                }
                Ok(Response::new(TriggerResponse {
                    success: outcome.failed.is_empty(),
                    failed_message_ids: outcome.failed,
                    error: String::new(),
                }))
            }
            Err(e) => {
                warn!(
                    app_id = %req.app_id,
                    queue = %req.queue_name,
                    error = %e,
                    "The pushed batch never ran; every message in it is handed back unhandled"
                );
                Ok(Response::new(TriggerResponse {
                    success: false,
                    failed_message_ids: all_ids,
                    error: e.to_string(),
                }))
            }
        }
    }

    async fn on_scheduled(
        &self,
        request: Request<ScheduledEvent>,
    ) -> Result<Response<TriggerResponse>, Status> {
        let req = request.into_inner();
        info!(
            app_id = %req.app_id,
            cron = %req.cron,
            scheduled_time = req.scheduled_time,
            "Scheduled trigger arrived"
        );

        match self
            .app_manager
            .execute_scheduled(&req.app_id, req.cron.clone(), req.scheduled_time)
            .await
        {
            Ok(()) => Ok(Response::new(TriggerResponse {
                success: true,
                failed_message_ids: Vec::new(),
                error: String::new(),
            })),
            Err(e) => {
                // Reported, not hidden behind a success: the control plane
                // moves on to another node when a tick does not run, and a
                // node that always answers "fine" turns a dead cron into
                // silence.
                warn!(
                    app_id = %req.app_id,
                    cron = %req.cron,
                    error = %e,
                    "The scheduled run did not happen on this node; this tick is not retried \
                     here and is never made up later"
                );
                Ok(Response::new(TriggerResponse {
                    success: false,
                    failed_message_ids: Vec::new(),
                    error: e.to_string(),
                }))
            }
        }
    }
}
