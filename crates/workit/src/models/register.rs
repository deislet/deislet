use hmac::Mac;
use nonce_auth::{NonceClient, ProtectionData};
use serde::{Deserialize, Serialize};

use crate::WorkerInfo;

/// Register request
#[derive(Debug, Serialize)]
pub struct RegisterRequest {
    /// (Associated) Instance Id
    pub id: String,
    /// Worker Info
    pub info: WorkerInfo,
    /// Authentication data
    pub protection_data: ProtectionData,
}

impl RegisterRequest {
    /// Create new register request
    pub fn new(id: String, worker_info: WorkerInfo, psk: &[u8]) -> Self {
        let client = NonceClient::new(psk);
        let protection_data: ProtectionData = client
            .create_protection_data(|mac, timestamp, nonce| {
                mac.update(timestamp.as_bytes());
                mac.update(nonce.as_bytes());
                mac.update(id.as_bytes());
                mac.update(serde_json::to_string(&worker_info).unwrap().as_bytes());
            })
            .unwrap_or_else(|e| {
                tracing::error!("Failed to create protection data: {}", e);
                panic!("Failed to create protection data");
            });

        Self {
            id,
            info: worker_info,
            protection_data,
        }
    }
}

/// Register response
#[derive(Debug, Deserialize)]
pub struct RegisterResponse {
    /// Success status
    pub success: bool,
    /// Response message
    pub message: String,
    /// Heartbeat URL (returned on success)
    pub heartbeat_url: Option<String>,
}
