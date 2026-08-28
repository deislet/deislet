use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Channel;

pub mod proto {
    tonic::include_proto!("deis.blob");
}

use proto::{blob_store_client::BlobStoreClient as GrpcBlobClient, *};

#[derive(Clone)]
pub struct BlobClient {
    client: Arc<Mutex<GrpcBlobClient<Channel>>>,
}

impl BlobClient {
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let client = GrpcBlobClient::connect(endpoint).await?;
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
        })
    }

    pub async fn get(&self, key: String) -> Result<Option<Vec<u8>>> {
        let request = tonic::Request::new(GetRequest { key });
        let mut client = self.client.lock().await;
        let response = client.get(request).await?.into_inner();

        if let Some(err) = response.error {
            return Err(anyhow::anyhow!("Blob error: {}", err));
        }

        Ok(response.data)
    }

    pub async fn put(&self, key: String, data: Vec<u8>) -> Result<()> {
        let request = tonic::Request::new(PutRequest { key, data });
        let mut client = self.client.lock().await;
        let response = client.put(request).await?.into_inner();

        if !response.success {
            let err = response
                .error
                .unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("Blob put failed: {}", err);
        }
        Ok(())
    }

    pub async fn delete(&self, key: String) -> Result<()> {
        let request = tonic::Request::new(DeleteRequest { key });
        let mut client = self.client.lock().await;
        let response = client.delete(request).await?.into_inner();

        if !response.success {
            let err = response
                .error
                .unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("Blob delete failed: {}", err);
        }
        Ok(())
    }
}
