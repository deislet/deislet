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
use std::sync::Arc;
use tokio::sync::Mutex;
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
    pub body: String,
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

#[derive(Clone)]
pub struct QueueClient {
    client: Arc<Mutex<GrpcQueueClient<Channel>>>,
    identity: StorageIdentity,
}

impl QueueClient {
    /// Connect eagerly, failing if deis-queue is not reachable right now.
    ///
    /// Used by tests and by callers that want the failure at construction.
    /// Application bindings use [`Self::connect_lazy`] instead, so that a
    /// queue outage cannot stop an app from loading and so that the call
    /// carries this tenant's capability token.
    pub async fn connect(endpoint: impl Into<String>, tenant: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let identity = StorageIdentity::new(tenant, None)?;
        let client = GrpcQueueClient::connect(endpoint.clone())
            .await
            .with_context(|| format!("connecting to the queue at {endpoint}"))?
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
        tracing::info!(%endpoint, app_id = identity.tenant(), "queue client connected");
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            identity,
        })
    }

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
            client: Arc::new(Mutex::new(client)),
            identity: identity.clone(),
        })
    }

    /// The tenant -- application and environment -- every call of this client
    /// is charged to.
    pub fn tenant(&self) -> &str {
        self.identity.tenant()
    }

    /// Enqueue one message, returning the id deis-queue assigned it.
    pub async fn send(&self, queue_name: String, message: String) -> Result<String> {
        let request = self.identity.stamp(SendRequest {
            queue_name: queue_name.clone(),
            message_body: message,
        });

        let mut client = self.client.lock().await;
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
        let request = self.identity.stamp(PullRequest {
            queue_name: queue_name.to_string(),
            max_messages: max_messages.min(i32::MAX as u32) as i32,
            lease_seconds: lease_seconds.min(i32::MAX as u32) as i32,
        });

        let mut client = self.client.lock().await;
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

        let mut client = self.client.lock().await;
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

        let mut client = self.client.lock().await;
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
