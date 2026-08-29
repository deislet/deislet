//! Queue client backing `ctx.services.queue` and the consumer poller.
//!
//! Talks to deis-queue, which partitions messages by application: the tenant
//! travels in the `x-deis-app-id` metadata header (see [`Tenant`]), not in the
//! proto messages, so one client speaks for exactly one application. A call
//! without the header is refused with `PermissionDenied` rather than served
//! from some default namespace, which is why this client cannot be built
//! without an app id.
//!
//! Delivery is at-least-once: a message stays in the queue until it is
//! acknowledged, so a consumer that crashes mid-batch sees the same messages
//! again once the lease lapses. Application queue handlers must be idempotent.

use super::tenant::Tenant;
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
    /// Handle for [`QueueClient::ack`] and [`QueueClient::nack`]. Stable
    /// across redeliveries, so it is also the key an idempotent handler can
    /// deduplicate on.
    pub id: String,
    pub body: String,
    /// Deliveries so far, including this one; starts at 1.
    pub attempts: i32,
    /// Unix second at which the lease lapses and the message becomes visible
    /// to other consumers again.
    pub lease_expires_at: i64,
    /// Unix second the message was enqueued.
    pub created_at: i64,
}

impl From<QueueMessage> for LeasedMessage {
    fn from(message: QueueMessage) -> Self {
        Self {
            id: message.id,
            body: message.body,
            attempts: message.attempts,
            lease_expires_at: message.lease_expires_at,
            created_at: message.created_at,
        }
    }
}

/// What a [`QueueClient::nack`] did with the messages handed back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NackOutcome {
    /// Messages that will be delivered again.
    pub released: i32,
    /// Messages that had used up their attempts and were moved to the dead
    /// letter table. These are never delivered again and need an operator.
    pub dead_lettered: i32,
}

#[derive(Clone)]
pub struct QueueClient {
    client: Arc<Mutex<GrpcQueueClient<Channel>>>,
    tenant: Tenant,
}

impl QueueClient {
    /// Connect eagerly, failing if deis-queue is not reachable right now.
    ///
    /// Used by tests and by callers that want the failure at construction.
    /// Application bindings use [`Self::connect_lazy`] instead, so that a
    /// queue outage cannot stop an app from loading.
    pub async fn connect(endpoint: impl Into<String>, app_id: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let tenant = Tenant::new(app_id)?;
        let client = GrpcQueueClient::connect(endpoint.clone())
            .await
            .with_context(|| format!("connecting to the queue at {endpoint}"))?
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
        tracing::info!(%endpoint, app_id = tenant.app_id(), "queue client connected");
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            tenant,
        })
    }

    /// Build a client that dials on first use.
    ///
    /// Fails only on an address that is not a URL or an app id that cannot be
    /// a gRPC header -- both configuration mistakes that no retry would fix.
    pub fn connect_lazy(endpoint: &str, app_id: impl Into<String>) -> Result<Self> {
        let tenant = Tenant::new(app_id)?;
        let channel = super::bindings::unary_channel(endpoint)?;
        let client = GrpcQueueClient::new(channel)
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
        tracing::debug!(
            endpoint,
            app_id = tenant.app_id(),
            "queue client prepared; it will dial on the first call"
        );
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            tenant,
        })
    }

    /// The application every call of this client is charged to.
    pub fn app_id(&self) -> &str {
        self.tenant.app_id()
    }

    /// Enqueue one message, returning the id deis-queue assigned it.
    pub async fn send(&self, queue_name: String, message: String) -> Result<String> {
        let request = self.tenant.stamp(SendRequest {
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
        let request = self.tenant.stamp(PullRequest {
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
    /// again. Returns how many were actually removed -- a lower number means
    /// some lease had already lapsed and the message went back into the
    /// queue or to the dead letter table.
    pub async fn ack(&self, queue_name: &str, message_ids: Vec<String>) -> Result<i32> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let request = self.tenant.stamp(AckRequest {
            queue_name: queue_name.to_string(),
            message_ids,
        });

        let mut client = self.client.lock().await;
        let response = client
            .ack(request)
            .await
            .with_context(|| format!("acknowledging messages on queue {queue_name:?}"))?;

        Ok(response.into_inner().acknowledged)
    }

    /// Hand messages back unhandled, visible again after `delay_seconds`.
    ///
    /// A message that has used up its attempts is dead lettered by the server
    /// instead of being released; the returned counts say which happened.
    pub async fn nack(
        &self,
        queue_name: &str,
        message_ids: Vec<String>,
        delay_seconds: u32,
    ) -> Result<NackOutcome> {
        if message_ids.is_empty() {
            return Ok(NackOutcome {
                released: 0,
                dead_lettered: 0,
            });
        }
        let request = self.tenant.stamp(NackRequest {
            queue_name: queue_name.to_string(),
            message_ids,
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
        })
    }
}
