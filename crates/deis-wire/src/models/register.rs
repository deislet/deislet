use serde::{Deserialize, Serialize};

use crate::NodeInfo;

/// Register request (simplified - authentication happens at connection level)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// (Associated) Instance Id  
    pub id: String,
    /// Node Info
    pub info: NodeInfo,
}

impl RegisterRequest {
    /// Create new register request (no authentication data needed)
    pub fn new(id: String, node_info: NodeInfo, _psk: &[u8]) -> Self {
        Self {
            id,
            info: node_info,
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
