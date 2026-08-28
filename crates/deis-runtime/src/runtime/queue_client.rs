use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Channel;

pub mod proto {
    tonic::include_proto!("deis.queue");
}

use proto::{queue_client::QueueClient as GrpcQueueClient, *};

#[derive(Clone)]
pub struct QueueClient {
    client: Arc<Mutex<GrpcQueueClient<Channel>>>,
}

impl QueueClient {
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let client = GrpcQueueClient::connect(endpoint)
            .await?
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_SIZE)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_SIZE);
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
        })
    }

    pub async fn send(&self, queue_name: String, message: String) -> Result<String> {
        let request = tonic::Request::new(SendRequest {
            queue_name,
            message_body: message,
        });

        let mut client = self.client.lock().await;
        let response = client.send(request).await?;
        let inner = response.into_inner();

        if !inner.success {
            anyhow::bail!("Queue send failed: {}", inner.error);
        }

        Ok(inner.message_id)
    }
}
