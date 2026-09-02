//! `ctx.services.queue.send` and `.sendBatch`: the producer half of queueing.
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

/// How many bytes of message bodies one batch may carry to deis-queue.
///
/// Not a policy of this platform's: it is the transport's own ceiling
/// ([`crate::MAX_GRPC_MESSAGE_BYTES`]), less a margin for the queue name, the
/// per-message framing and the metadata headers. The check exists here rather
/// than being left to tonic because tonic refuses an over-long message with a
/// protobuf encoder error, which tells an application author nothing about
/// what they did or what to do instead. deis-queue's own `max_batch_messages`
/// is still the ceiling on how *many* messages a batch may hold; this is the
/// one the wire imposes on how *large* they are together.
const MAX_BATCH_BYTES: usize = crate::MAX_GRPC_MESSAGE_BYTES - 64 * 1024;

/// What `send` and `sendBatch` answer when this app has no queue client.
///
/// It names the consequence rather than only the condition: the application
/// is holding a rejected promise and has to know that the message is not
/// queued anywhere and will not be delivered later.
use super::ops::escape_control;

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
#[op2]
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
        // The name is the application's own string and this line lands in
        // the node's log file, where a newline ends the line and starts one
        // the application wrote in full - the same forgery `emit_app_log`
        // escapes against, escaped by the same rule.
        tracing::warn!(
            target: "app",
            app_id,
            queue = %escape_control(&queue_name),
            "Refused a send to queue {}: no deis-queue is bound to this app, so the message was \
             not enqueued and the app was told so. Declare \"queue\" in the manifest and give the \
             deployment a queue address",
            escape_control(&queue_name)
        );
        return Err(queue_unavailable(&queue_name));
    };

    client
        .send(queue_name, message.to_vec())
        .await
        .map_err(|e| service_call_failed("queue.send", e))
}

/// Enqueue a whole array of messages in one call, as one unit.
///
/// # Why this is not `send` in a loop
///
/// Two things a loop cannot give an application. deis-queue writes the batch
/// in one SQLite transaction, so either every message is stored or none is --
/// there is no state in which half an array arrived and the application has
/// to work out which half. And it is *one* op dispatch: fifty messages sent
/// one at a time are fifty dispatches against `rate_limit_ops_per_sec`, fifty
/// round trips to deis-queue, and fifty chances for the invocation's budget to
/// run out part way through.
///
/// # How the bodies travel
///
/// One buffer holding every body end to end, plus the length of each. A queue
/// body is bytes (see [`op_queue_send`]), and an array of buffers would be one
/// V8-to-Rust conversion per message; framing them into a single buffer that
/// `js/runtime.js` builds once costs one. The lengths are what split it back
/// apart, so they and the buffer are two halves of one argument: a mismatch
/// between them is a bug in `js/runtime.js` and is refused rather than
/// guessed at, because a wrong split would enqueue bodies no application ever
/// wrote.
#[op2]
async fn op_queue_send_batch(
    state: Rc<RefCell<OpState>>,
    #[string] queue_name: String,
    #[buffer] bodies: JsBuffer,
    #[serde] lengths: Vec<u32>,
) -> Result<(), JsErrorBox> {
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
        // Escaped for the same reason as the single-send refusal above.
        tracing::warn!(
            target: "app",
            app_id,
            queue = %escape_control(&queue_name),
            messages = lengths.len(),
            "Refused a batch of {} message(s) to queue {}: no deis-queue is bound to this app, \
             so none of them was enqueued and the app was told so. Declare \"queue\" in the \
             manifest and give the deployment a queue address",
            lengths.len(),
            escape_control(&queue_name)
        );
        return Err(queue_unavailable(&queue_name));
    };

    let total: usize = lengths.iter().map(|len| *len as usize).sum();
    if total != bodies.len() {
        return Err(JsErrorBox::generic(format!(
            "queue.sendBatch({queue_name:?}): the {} message length(s) add up to {total} bytes \
             but {} bytes of bodies were handed over; nothing was enqueued",
            lengths.len(),
            bodies.len()
        )));
    }

    if total > MAX_BATCH_BYTES {
        tracing::warn!(
            target: "app",
            app_id,
            queue = %escape_control(&queue_name),
            messages = lengths.len(),
            bytes = total,
            limit = MAX_BATCH_BYTES,
            "Refused a batch of {} message(s) to queue {}: their bodies come to {} bytes and one \
             gRPC message may carry {}. Nothing was enqueued; send the batch in smaller pieces \
             or put the payloads in blob storage and queue their keys",
            lengths.len(),
            escape_control(&queue_name),
            total,
            MAX_BATCH_BYTES
        );
        return Err(JsErrorBox::generic(format!(
            "queue.sendBatch({queue_name:?}): the {} message bodies come to {total} bytes, and a \
             batch may carry {MAX_BATCH_BYTES}. Nothing was enqueued",
            lengths.len()
        )));
    }

    // Split once, into owned bodies, because that is what the proto message
    // takes. The buffer is this invocation's own and is dropped straight
    // after.
    let bytes = bodies.to_vec();
    let mut messages = Vec::with_capacity(lengths.len());
    let mut offset = 0usize;
    for len in &lengths {
        let end = offset + *len as usize;
        messages.push(bytes[offset..end].to_vec());
        offset = end;
    }

    let ids = client
        .send_batch(queue_name.clone(), messages)
        .await
        .map_err(|e| service_call_failed("queue.sendBatch", e))?;

    // Debug rather than info: a healthy application may do this on every
    // request, and the interesting events -- a refusal, a short answer -- are
    // already errors by the time they get here.
    tracing::debug!(
        target: "app",
        app_id,
        queue = %escape_control(&queue_name),
        messages = ids.len(),
        bytes = total,
        "Enqueued {} message(s) to queue {} as one transaction",
        ids.len(),
        escape_control(&queue_name)
    );
    Ok(())
}

extension!(
    deis_runtime_queue_ops,
    ops = [op_queue_send, op_queue_send_batch],
);

pub fn init_queue_extension() -> deno_core::Extension {
    deis_runtime_queue_ops::init()
}

#[cfg(test)]
#[path = "queue_ops_test.rs"]
mod queue_ops_test;
