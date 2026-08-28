use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeRole {
    #[serde(rename = "runtime")]
    Runtime(String),
    #[serde(rename = "proxy")]
    Proxy(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub node_role: NodeRole,
    pub listen_host: String,
    pub listen_port: Vec<u16>,
}

/// Authentication data - matches deis-wire SDK format
/// Uses HMAC-SHA256 signature for secure authentication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthData {
    /// Associated ID (Node Instance ID)
    pub associated_id: String,
    /// Authentication signature: SHA256(secret + associated_id + timestamp)
    pub signature: String,
    /// Timestamp (to prevent replay attacks)
    pub timestamp: u64,
}

impl AuthData {
    /// Validate authentication data against a secret
    /// Returns true if signature is valid and timestamp is within 5 minutes
    pub fn validate(&self, secret: &str) -> bool {
        // Check if timestamp is within acceptable window (5 minutes)
        let current_timestamp = chrono::Utc::now().timestamp() as u64;
        if current_timestamp.saturating_sub(self.timestamp) > 300 {
            tracing::warn!(
                "Auth timestamp expired: current={}, provided={}, diff={}",
                current_timestamp,
                self.timestamp,
                current_timestamp.saturating_sub(self.timestamp)
            );
            return false;
        }

        // Validate signature: SHA256(secret + associated_id + timestamp)
        let mut hasher = Sha256::new();
        hasher.update(secret.as_bytes());
        hasher.update(self.associated_id.as_bytes());
        hasher.update(self.timestamp.to_be_bytes());
        let expected_signature = format!("{:x}", hasher.finalize());

        if self.signature != expected_signature {
            tracing::warn!(
                "Auth signature mismatch for {}: expected={}, got={}",
                self.associated_id,
                &expected_signature[..16],
                &self.signature[..16.min(self.signature.len())]
            );
            return false;
        }

        true
    }
}

/// Authentication response - matches deis-wire SDK format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    pub authenticated: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub id: String,
    pub info: NodeInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub id: String,
    pub power_reserve: u8,
}

/// Heartbeat acknowledgment - matches deis-wire SDK format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatAck {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_heartbeat_interval: Option<u64>,
}

/// Message type enum
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    ConfigUpdate,
    ConfigUpdateAck,
    Command,
    CommandResponse,
}

/// Generic message wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub message_type: MessageType,
    pub data: serde_json::Value,
}

/// Config update message (control -> node)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUpdateMessage {
    pub node_id: String,
    pub configs: std::collections::HashMap<String, serde_json::Value>,
    pub version_timestamp: chrono::DateTime<chrono::Utc>,
}

/// Config update acknowledgment (node -> control)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUpdateAck {
    pub success: bool,
    pub message: String,
    pub applied_version: chrono::DateTime<chrono::Utc>,
}
