//! `ctx.services.queue.send`: the producer half of queueing.
//!
//! Consumption is the other half and does not go through an op at all: the
//! node polls deis-queue and calls the app's `queue` handler, see
//! `super::queue_consumer`.
//!
//! # There is no local queue
//!
//! An app whose queue client is missing -- it did not declare `queue` in its
//! manifest, or the control plane sent no deis-queue address -- gets an error
//! from `send`, never a receipt. Until wave B this op fell back to an
//! in-process vector and answered with a made-up message id, so the promise
//! resolved, the app moved on, and the message vanished with the isolate. That
//! failure is invisible from inside the application and from the queue: the
//! producer looks healthy and the consumer simply never receives anything. A
//! rejected send, by contrast, is something an app can retry, log, or fail on.

use super::ops::{service_call_failed, AppId};
use super::queue_client::QueueClient;
use deno_core::{extension, op2, JsBuffer, OpState};
use deno_error::JsErrorBox;
use std::cell::RefCell;
use std::rc::Rc;

/// What `send` answers when this app has no queue client.
///
/// It names the consequence rather than only the condition: the application
/// is holding a rejected promise and has to know that the message is not
/// queued anywhere and will not be delivered later.
fn queue_unavailable(queue_name: &str) -> JsErrorBox {
    JsErrorBox::generic(format!(
        "queue service is not available to this app: the message for queue {queue_name:?} was \
         not enqueued. Either the manifest does not declare \"queue\" or the deployment carried \
         no deis-queue address"
    ))
}

/// Enqueue one message, answering with the id deis-queue assigned it.
///
/// The body arrives as a buffer rather than a string: a queue carries
/// whatever the application put on it, and the `Queue` class in
/// `js/runtime.js` is what turns a string or an object into those bytes. See
/// `blob_ops.rs` for why `#[buffer]` and not `#[serde] Vec<u8>`.
///
/// The id comes from the server. Nothing here invents one: an id the queue
/// never issued would let an app believe a message exists that it could never
/// find again. The JS binding does not hand it to the application -- `send()`
/// resolves to nothing, the way Cloudflare's does -- so what an app learns
/// from this op is only whether the queue took the message.
#[op2(async)]
#[string]
async fn op_queue_send(
    state: Rc<RefCell<OpState>>,
    #[string] queue_name: String,
    #[buffer] message: JsBuffer,
) -> Result<String, JsErrorBox> {
    let (client_opt, app_id) = {
        let state_borrow = state.borrow();
        (
            state_borrow.try_borrow::<QueueClient>().cloned(),
            state_borrow
                .try_borrow::<AppId>()
                .map(|app| app.0.clone())
                .unwrap_or_default(),
        )
    };

    let Some(client) = client_opt else {
        // Attributed like every other line an app's own code causes, so it
        // reaches deis-telemetry under the tenant that tried to send; see
        // `AppId`.
        tracing::warn!(
            target: "app",
            app_id,
            queue = %queue_name,
            "Refused a send to queue {}: no deis-queue is bound to this app, so the message was \
             not enqueued and the app was told so. Declare \"queue\" in the manifest and give the \
             deployment a queue address",
            queue_name
        );
        return Err(queue_unavailable(&queue_name));
    };

    client
        .send(queue_name, message.to_vec())
        .await
        .map_err(|e| service_call_failed("queue.send", e))
}

extension!(deis_runtime_queue_ops, ops = [op_queue_send],);

pub fn init_queue_extension() -> deno_core::Extension {
    deis_runtime_queue_ops::init()
}
