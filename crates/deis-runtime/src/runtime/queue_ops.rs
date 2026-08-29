//! `ctx.services.queue.send`: the producer half of queueing.
//!
//! Consumption is the other half and does not go through an op at all: the
//! node polls deis-queue and calls the app's `queue` handler, see
//! `super::queue_consumer`.

use super::ops::{service_call_failed, service_unavailable};
use super::queue_client::QueueClient;
use deno_core::{extension, op2, OpState};
use deno_error::JsErrorBox;
use std::cell::RefCell;
use std::rc::Rc;

/// In-memory queue used when no deis-queue client is bound.
///
/// It exists so a single-process development run can exercise a producer
/// without a queue server. Messages pushed here are visible to nothing and
/// vanish with the isolate, which is why binding a real queue logs and this
/// path does not pretend to have delivered anything durable.
pub type MemoryQueue = Rc<RefCell<Vec<(String, String)>>>;

#[op2(async)]
#[string]
async fn op_queue_send(
    state: Rc<RefCell<OpState>>,
    #[string] queue_name: String,
    #[string] message: String,
) -> Result<String, JsErrorBox> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<QueueClient>().cloned()
    };

    if let Some(client) = client_opt {
        return client
            .send(queue_name, message)
            .await
            .map_err(|e| service_call_failed("queue.send", e));
    }

    // Fallback to memory
    let state_borrow = state.borrow();
    if let Some(mem_queue) = state_borrow.try_borrow::<MemoryQueue>() {
        mem_queue.borrow_mut().push((queue_name.clone(), message));
        tracing::warn!(
            queue = %queue_name,
            "Queue message kept in this isolate's memory and will never be consumed: no \
             deis-queue is bound to this app. Declare \"queue\" in the manifest and give the \
             node a queue address, or the send is a no-op"
        );
        return Ok("mem-id".to_string());
    }

    Err(service_unavailable("queue"))
}

extension!(deis_runtime_queue_ops, ops = [op_queue_send],);

pub fn init_queue_extension() -> deno_core::Extension {
    deis_runtime_queue_ops::init()
}
