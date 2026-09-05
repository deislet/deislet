//! Queue client backing `ctx.services.queue` and the consumer poller.
//!
//! Talks to deis-queue, which partitions messages by tenant -- one
//! application in one environment: the tenant travels in the `x-deis-tenant`
//! metadata header, and the capability token that proves this node may act as
//! it in `x-deis-storage-token` (both stamped by [`StorageIdentity`]), not in
//! the proto messages, so one client
//! speaks for exactly one environment of one application and a preview never
//! consumes production's messages. A call without the header is refused with
//! `PermissionDenied` rather than served from some default namespace, which is
//! why this client cannot be built without a tenant.
//!
//! Delivery is at-least-once: a message stays in the queue until it is
//! acknowledged, so a consumer that crashes mid-batch sees the same messages
//! again once the lease lapses. Application queue handlers must be idempotent.

use super::bindings::StorageIdentity;
use anyhow::{Context, Result};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

pub mod proto {
    tonic::include_proto!("deis.queue");
}

use proto::{queue_client::QueueClient as GrpcQueueClient, *};

/// A message leased from deis-queue, in the shape the runtime passes around.
///
/// Mirrors [`proto::QueueMessage`] so callers outside this module do not have
/// to name generated types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasedMessage {
    /// The message's identity, stable across redeliveries, so it is the key an
    /// idempotent handler can deduplicate on. It is *not* on its own enough to
    /// answer for the message; see [`Self::handle`].
    pub id: String,
    /// The bytes that were sent, handed back untouched. Bytes and not a
    /// string because a queue body is whatever the application put on it --
    /// a compressed record, a serialised frame -- and only the consumer knows
    /// how to read it. See [`Self::js_body`] for the form a handler sees.
    pub body: Vec<u8>,
    /// Deliveries so far, including this one; starts at 1.
    pub attempts: i32,
    /// Unix second at which the lease lapses and the message becomes visible
    /// to other consumers again.
    pub lease_expires_at: i64,
    /// Unix second the message was enqueued.
    pub created_at: i64,
    /// Proof that this delivery is the current one, issued fresh by deis-queue
    /// every time the message is handed out. Opaque here; the only thing this
    /// crate does with it is hand it back.
    pub lease_token: String,
}

impl LeasedMessage {
    /// What [`QueueClient::ack`] and [`QueueClient::nack`] take.
    ///
    /// The id alone is not enough on purpose. A consumer whose lease lapsed
    /// while it was still working has had its message handed to somebody else;
    /// answering by id would then delete or release a delivery that another
    /// consumer is in the middle of, destroying work with nothing to show for
    /// it. The token names *this* delivery, so a late answer is refused
    /// instead.
    pub fn handle(&self) -> MessageHandle {
        MessageHandle {
            id: self.id.clone(),
            lease_token: self.lease_token.clone(),
        }
    }

    /// The body in the form the JS payload carries it.
    ///
    /// A queue batch reaches an isolate as JSON written into the source text
    /// of the invocation script, and JSON has no way to spell a byte that is
    /// not text. So a body that is valid UTF-8 travels as itself -- which is
    /// every body an application has ever sent, and what `message.body` in
    /// `js/runtime.js` hands the handler -- and anything else travels base64
    /// encoded, to be turned back into bytes there. The two cases are
    /// distinguished by which variant this is, never by inspecting the
    /// string: a text body that happened to look like base64 would otherwise
    /// be decoded into something the sender never wrote.
    pub fn js_body(&self) -> JsBody {
        match std::str::from_utf8(&self.body) {
            Ok(text) => JsBody::Text {
                text: text.to_string(),
            },
            Err(_) => JsBody::Bytes {
                base64: base64_standard(&self.body),
            },
        }
    }
}

/// One message body as the JS payload carries it; see
/// [`LeasedMessage::js_body`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum JsBody {
    /// The body is valid UTF-8 and travels as the text itself.
    Text { text: String },
    /// The body is not text and travels base64 encoded.
    Bytes { base64: String },
}

/// Standard base64 with padding, the alphabet `js/runtime.js`'s `atob`
/// decodes.
fn base64_standard(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

impl From<QueueMessage> for LeasedMessage {
    fn from(message: QueueMessage) -> Self {
        Self {
            id: message.id,
            body: message.body,
            attempts: message.attempts,
            lease_expires_at: message.lease_expires_at,
            created_at: message.created_at,
            lease_token: message.lease_token,
        }
    }
}

/// One message a consumer is answering for, and its proof of holding it.
///
/// Mirrors [`proto::MessageHandle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageHandle {
    pub id: String,
    pub lease_token: String,
}

impl From<MessageHandle> for proto::MessageHandle {
    fn from(handle: MessageHandle) -> Self {
        Self {
            id: handle.id,
            lease_token: handle.lease_token,
        }
    }
}

/// What a [`QueueClient::ack`] did with the messages reported as handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckOutcome {
    /// Messages deleted; they are never delivered again.
    pub acknowledged: i32,
    /// Messages whose lease had already lapsed and which now belong to another
    /// consumer. They were left alone, so the same work is being done twice
    /// right now: this consumer finished it, and whoever holds the message now
    /// is doing it again.
    pub stale_leases: i32,
}

/// What a [`QueueClient::nack`] did with the messages handed back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NackOutcome {
    /// Messages that will be delivered again.
    pub released: i32,
    /// Messages that had used up their attempts and were moved to the dead
    /// letter table. These are never delivered again and need an operator.
    pub dead_lettered: i32,
    /// Messages whose lease had already lapsed; see [`AckOutcome::stale_leases`].
    pub stale_leases: i32,
}

/// Queue client for one tenant.
///
/// Cloning is cheap and every call clones: the inner tonic client wraps an
/// HTTP/2 `Channel`, which is a reference-counted handle that multiplexes
/// concurrent calls over one connection. One shared client behind a lock
/// would serialise this tenant's queue traffic, so a consumer's slow `Pull`
/// would hold up a request handler's `send` on the same node.
#[derive(Clone)]
pub struct QueueClient {
    client: GrpcQueueClient<Channel>,
    identity: StorageIdentity,
    consumer_generation: Option<MetadataValue<tonic::metadata::Ascii>>,
}

impl QueueClient {
    /// Build a client that dials on first use.
    ///
    /// Fails only on an address that is not a URL -- a configuration mistake
    /// that no retry would fix.
    pub(crate) fn connect_lazy(endpoint: &str, identity: &StorageIdentity) -> Result<Self> {
        let channel = super::bindings::unary_channel(endpoint)?;
        let client = GrpcQueueClient::new(channel)
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
        tracing::debug!(
            endpoint,
            app_id = identity.tenant(),
            "queue client prepared; it will dial on the first call"
        );
        Ok(Self {
            client,
            identity: identity.clone(),
            consumer_generation: None,
        })
    }

    /// Bind pull admission to one immutable deployment generation. Only the
    /// runtime's background consumer uses this; application producer clients
    /// remain unable to choose or advance the fence.
    pub(crate) fn with_consumer_generation(mut self, generation: Option<&str>) -> Result<Self> {
        self.consumer_generation = generation
            .map(|value| {
                value
                    .parse()
                    .with_context(|| "deployment generation is not valid gRPC metadata")
            })
            .transpose()?;
        Ok(self)
    }

    /// The tenant -- application and environment -- every call of this client
    /// is charged to.
    pub fn tenant(&self) -> &str {
        self.identity.tenant()
    }

    /// Enqueue one message, returning the id deis-queue assigned it.
    ///
    /// The body is bytes: the queue stores and returns exactly what it is
    /// given, and an application that sends text encodes it first.
    pub async fn send(&self, queue_name: String, message: Vec<u8>) -> Result<String> {
        let request = self.identity.stamp(SendRequest {
            queue_name: queue_name.clone(),
            message_body: message,
        });

        let mut client = self.client.clone();
        let response = client
            .send(request)
            .await
            .with_context(|| format!("sending to queue {queue_name:?}"))?;
        let inner = response.into_inner();

        // The server reports refusals as a gRPC status, so an OK response
        // always carries success = true. The check stays because a future
        // server could start using the flag, and a message believed sent but
        // never stored is the worst failure this call has.
        if !inner.success {
            anyhow::bail!(
                "queue {queue_name:?} refused the message: {}",
                if inner.error.is_empty() {
                    "no reason given"
                } else {
                    &inner.error
                }
            );
        }

        Ok(inner.message_id)
    }

    /// Enqueue several messages as one unit, returning the ids deis-queue
    /// assigned them in the order the bodies were given.
    ///
    /// **The batch is atomic.** deis-queue writes the whole of it in one
    /// SQLite transaction (`deis_queue::queue::Queue::send_batch`), so a
    /// refused batch leaves nothing behind: an application that is told the
    /// call failed can retry the same array without first working out which
    /// half of it arrived. `send` in a loop cannot offer that, and it is why
    /// this exists beside it rather than as a convenience wrapper.
    ///
    /// The ceiling on how many messages one batch may carry is the server's
    /// (`max_batch_messages`, 100 by default), not this client's: a caller
    /// that could set its own would be setting how long one producer may hold
    /// the queue's write lock.
    pub async fn send_batch(
        &self,
        queue_name: String,
        messages: Vec<Vec<u8>>,
    ) -> Result<Vec<String>> {
        let sent = messages.len();
        let request = self.identity.stamp(SendBatchRequest {
            queue_name: queue_name.clone(),
            messages,
        });

        let mut client = self.client.clone();
        let response = client
            .send_batch(request)
            .await
            .with_context(|| format!("sending {sent} message(s) to queue {queue_name:?}"))?;
        let inner = response.into_inner();

        // Same reasoning as `send`: a refusal arrives as a gRPC status, so an
        // OK response always carries success = true. Checked anyway, because a
        // batch believed stored and never written is this call's worst
        // failure.
        if !inner.success {
            anyhow::bail!(
                "queue {queue_name:?} refused a batch of {sent} message(s): {}",
                if inner.error.is_empty() {
                    "no reason given"
                } else {
                    &inner.error
                }
            );
        }

        // One id per message is what makes the batch atomic observable from
        // here. A server that stored some and answered with fewer ids would
        // have broken the all-or-nothing promise this method is sold on, and
        // an application told "sent" would never find out which ones are
        // missing; failing loudly is the only answer that keeps the promise
        // honest.
        if inner.message_ids.len() != sent {
            anyhow::bail!(
                "queue {queue_name:?} was given {sent} message(s) and answered with {} id(s); \
                 the batch is meant to be all-or-nothing, so which of them are stored is now \
                 unknown",
                inner.message_ids.len()
            );
        }

        Ok(inner.message_ids)
    }

    /// Lease up to `max_messages` messages for `lease_seconds`.
    ///
    /// Returns an empty vector when the queue has nothing ready; that is the
    /// normal case for a poller, not a failure. The call never waits for work
    /// to arrive, so a consumer that stops polling holds nothing on the
    /// server.
    pub async fn pull(
        &self,
        queue_name: &str,
        max_messages: u32,
        lease_seconds: u32,
    ) -> Result<Vec<LeasedMessage>> {
        let mut request = self.identity.stamp(PullRequest {
            queue_name: queue_name.to_string(),
            max_messages: max_messages.min(i32::MAX as u32) as i32,
            lease_seconds: lease_seconds.min(i32::MAX as u32) as i32,
        });
        if let Some(generation) = &self.consumer_generation {
            request.metadata_mut().insert(
                deis_wire::conventions::DEPLOYMENT_GENERATION_HEADER,
                generation.clone(),
            );
        }

        let mut client = self.client.clone();
        let response = client
            .pull(request)
            .await
            .with_context(|| format!("pulling from queue {queue_name:?}"))?;

        Ok(response
            .into_inner()
            .messages
            .into_iter()
            .map(LeasedMessage::from)
            .collect())
    }

    /// Report messages as handled; they are deleted and never delivered
    /// again.
    ///
    /// Takes handles rather than ids: the lease token in each one is what
    /// proves this consumer still holds the delivery it is answering for. A
    /// handle whose lease has lapsed is counted in
    /// [`AckOutcome::stale_leases`] and its row is left alone.
    pub async fn ack(&self, queue_name: &str, handles: Vec<MessageHandle>) -> Result<AckOutcome> {
        if handles.is_empty() {
            return Ok(AckOutcome {
                acknowledged: 0,
                stale_leases: 0,
            });
        }
        let request = self.identity.stamp(AckRequest {
            queue_name: queue_name.to_string(),
            messages: handles.into_iter().map(Into::into).collect(),
        });

        let mut client = self.client.clone();
        let response = client
            .ack(request)
            .await
            .with_context(|| format!("acknowledging messages on queue {queue_name:?}"))?;
        let inner = response.into_inner();

        Ok(AckOutcome {
            acknowledged: inner.acknowledged,
            stale_leases: inner.stale_leases,
        })
    }

    /// Hand messages back unhandled, visible again after `delay_seconds`.
    ///
    /// A message that has used up its attempts is dead lettered by the server
    /// instead of being released; the returned counts say which happened.
    pub async fn nack(
        &self,
        queue_name: &str,
        handles: Vec<MessageHandle>,
        delay_seconds: u32,
    ) -> Result<NackOutcome> {
        if handles.is_empty() {
            return Ok(NackOutcome {
                released: 0,
                dead_lettered: 0,
                stale_leases: 0,
            });
        }
        let request = self.identity.stamp(NackRequest {
            queue_name: queue_name.to_string(),
            messages: handles.into_iter().map(Into::into).collect(),
            delay_seconds: delay_seconds.min(i32::MAX as u32) as i32,
        });

        let mut client = self.client.clone();
        let response = client
            .nack(request)
            .await
            .with_context(|| format!("returning messages to queue {queue_name:?}"))?;
        let inner = response.into_inner();

        Ok(NackOutcome {
            released: inner.released,
            dead_lettered: inner.dead_lettered,
            stale_leases: inner.stale_leases,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leased(body: Vec<u8>) -> LeasedMessage {
        LeasedMessage {
            id: "m1".to_string(),
            body,
            attempts: 1,
            lease_expires_at: 0,
            created_at: 0,
            lease_token: "lease".to_string(),
        }
    }

    #[test]
    fn a_text_body_travels_as_itself_and_anything_else_as_base64() {
        assert_eq!(
            leased(b"{\"job\":1}".to_vec()).js_body(),
            JsBody::Text {
                text: "{\"job\":1}".to_string()
            }
        );
        // Two bytes no UTF-8 decoder accepts. RFC 4648 base64 of ff fe.
        assert_eq!(
            leased(vec![0xff, 0xfe]).js_body(),
            JsBody::Bytes {
                base64: "//4=".to_string()
            }
        );
    }

    #[test]
    fn a_text_body_that_looks_like_base64_still_travels_as_text() {
        // The two forms are told apart by which variant this is, never by
        // inspecting the string: `js/runtime.js` decodes only the base64
        // variant, so this application's own text is handed over unchanged
        // rather than turned into bytes it never wrote.
        assert_eq!(
            leased(b"aGVsbG8=".to_vec()).js_body(),
            JsBody::Text {
                text: "aGVsbG8=".to_string()
            }
        );
    }

    #[test]
    fn the_two_forms_serialise_to_the_shapes_the_js_side_reads() {
        // `js/runtime.js` looks for `text` first and falls back to `base64`;
        // a rename on either side would leave every body arriving as bytes.
        assert_eq!(
            serde_json::to_string(&leased(b"hi".to_vec()).js_body()).unwrap(),
            r#"{"text":"hi"}"#
        );
        assert_eq!(
            serde_json::to_string(&leased(vec![0xff, 0xfe]).js_body()).unwrap(),
            r#"{"base64":"//4="}"#
        );
    }

    #[test]
    fn an_empty_body_is_text_rather_than_bytes() {
        // A zero-length body decodes as the empty string, so a handler reads
        // `""` rather than being pushed onto the bytes path for nothing.
        assert_eq!(
            leased(Vec::new()).js_body(),
            JsBody::Text {
                text: String::new()
            }
        );
    }
}
