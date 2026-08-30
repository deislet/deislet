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
//!   calls this. The answer separates "this node could not run it", which is
//!   worth trying another node, from "the application's handler ran and
//!   failed", which is not: the handler has already had whatever effect it
//!   managed, and another node would repeat it. A tick is never made up later.
//! * **`OnQueue`** - a batch pushed in from outside, rather than leased by
//!   this node's own poller. The response names the messages the handler did
//!   not complete, and the caller owns acknowledging the rest: this node holds
//!   no lease it could act on.

use super::proto::trigger_server::Trigger;
use super::proto::*;
use crate::app::{AppKey, ApplicationManager, ScheduledFailure};
use crate::runtime::LeasedMessage;
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

/// Marker at the front of `TriggerResponse.error` when the application's own
/// handler ran and failed, as opposed to this node being unable to run it.
///
/// The distinction decides whether the caller should try another node. It
/// travels in the error text because `TriggerResponse` has no field for it:
/// adding one to trigger.proto, so this does not depend on a prefix, is the
/// proper fix and is follow-up work in the control-plane track.
pub const HANDLER_FAILED_PREFIX: &str = "handler-failed: ";

pub struct TriggerServiceImpl {
    app_manager: Arc<ApplicationManager>,
}

impl TriggerServiceImpl {
    pub fn new(app_manager: Arc<ApplicationManager>) -> Self {
        Self { app_manager }
    }

    /// Which running application a trigger is about.
    ///
    /// A trigger names an application and no environment, so it means the
    /// default one - the same rule the HTTP data path applies to a request
    /// with no environment header.
    fn target(&self, app_id: &str) -> AppKey {
        AppKey::production(app_id)
    }

    /// Which running application a scheduled trigger is about.
    ///
    /// `ScheduledEvent` now carries the environment, because the control plane
    /// keys its schedules by (app, environment) and a node runs production and
    /// every preview of an app side by side. An empty field is a caller that
    /// predates it and means the default environment - the same rule the HTTP
    /// data path applies to a request with no environment header - and that is
    /// the case worth warning about: if this node also runs other environments
    /// of the app, the tick may belong to one of them and is about to run
    /// production's code with production's secrets.
    fn scheduled_target(&self, app_id: &str, environment: &str, cron: &str) -> AppKey {
        let environment = environment.trim();
        if !environment.is_empty() {
            return AppKey::new(app_id, environment);
        }

        let key = self.target(app_id);
        let others: Vec<String> = self
            .app_manager
            .environments_of(app_id)
            .into_iter()
            .filter(|candidate| *candidate != key)
            .map(|candidate| candidate.environment().to_string())
            .collect();
        if !others.is_empty() {
            warn!(
                app_id = %app_id,
                cron = %cron,
                other_environments = ?others,
                "A scheduled trigger arrived with no environment, so it is being run against \
                 {}. This node also runs {} other environment(s) of this app, and a schedule \
                 that belongs to one of them is running the wrong code with the wrong secrets. \
                 The caller is older than ScheduledEvent.environment and should be upgraded",
                key,
                others.len()
            );
        }
        key
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
        // The event names no environment, so it means the one a caller means
        // when it does not say. See `scheduled_target` for what that costs.
        let key = self.target(&req.app_id);

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
                // trigger.proto still types this leg's body as a string, so
                // a push can only carry text; the queue itself carries bytes.
                body: message.body.into_bytes(),
                attempts: message.attempts,
                // This node never leased these messages, so it has no lease to
                // report and nothing it could ack or nack: no expiry and no
                // lease token. Whoever pushed them holds those and reads the
                // answer below.
                lease_expires_at: 0,
                lease_token: String::new(),
                created_at: message.timestamp,
            })
            .collect();

        match self
            .app_manager
            .execute_queue_batch(&key, req.queue_name.clone(), messages)
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

    /// Run one due cron occurrence, and say which kind of failure it was.
    ///
    /// The caller walks the fleet until a node reports success, so the two
    /// failures must not look alike. "This node cannot run it" is worth
    /// another node. "The app's handler threw" is not: the same code fails
    /// the same way everywhere, and by then the handler has already had
    /// whatever effect it had before it failed - a queue send, a write - so
    /// every extra node repeats it. A handler that sends a settlement message
    /// and then throws would send one per node in the fleet.
    async fn on_scheduled(
        &self,
        request: Request<ScheduledEvent>,
    ) -> Result<Response<TriggerResponse>, Status> {
        let req = request.into_inner();
        let key = self.scheduled_target(&req.app_id, &req.environment, &req.cron);
        info!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            cron = %req.cron,
            scheduled_time = req.scheduled_time,
            "Scheduled trigger arrived"
        );

        match self
            .app_manager
            .execute_scheduled(&key, req.cron.clone(), req.scheduled_time)
            .await
        {
            Ok(()) => Ok(Response::new(TriggerResponse {
                success: true,
                failed_message_ids: Vec::new(),
                error: String::new(),
            })),
            Err(ScheduledFailure::NotRunnableHere(reason)) => {
                // Reported, not hidden behind a success: the control plane
                // moves on to another node when a tick does not run, and a
                // node that always answers "fine" turns a dead cron into
                // silence.
                warn!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    cron = %req.cron,
                    error = %reason,
                    "This node cannot run the scheduled handler of {}, so the tick did not \
                     happen here; another node should be tried",
                    key
                );
                Ok(Response::new(TriggerResponse {
                    success: false,
                    failed_message_ids: Vec::new(),
                    error: reason,
                }))
            }
            Err(ScheduledFailure::HandlerFailed(reason)) => {
                warn!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    cron = %req.cron,
                    error = %reason,
                    "The scheduled handler of {} ran here and failed. This occurrence has had \
                     whatever effect the handler managed before it failed, so it must not be \
                     retried on another node; it is not made up later either",
                    key
                );
                Ok(Response::new(TriggerResponse {
                    success: false,
                    failed_message_ids: Vec::new(),
                    error: format!("{HANDLER_FAILED_PREFIX}{reason}"),
                }))
            }
        }
    }
}

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
