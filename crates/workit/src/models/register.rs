use serde::{Deserialize, Serialize};

use crate::WorkerInfo;

/// Register request (simplified - authentication happens at connection level)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// (Associated) Instance Id  
    pub id: String,
    /// Worker Info
    pub info: WorkerInfo,
}

impl RegisterRequest {
    /// Create new register request (no authentication data needed)
    pub fn new(id: String, worker_info: WorkerInfo, _psk: &[u8]) -> Self {
        Self {
            id,
            info: worker_info,
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
