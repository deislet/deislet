use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Simplified WebSocket authentication data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthData {
    /// Associated ID (Worker Instance ID)
    pub associated_id: String,
    /// Authentication signature (used only once at connection time)
    pub signature: String,
    /// Timestamp (to prevent auth data expiration)
    pub timestamp: u64,
}

impl AuthData {
    /// Create authentication data (used only when establishing WebSocket connection)
    pub fn new(associated_id: String, secret: &[u8]) -> Result<Self, crate::WorkitError> {
        let timestamp = chrono::Utc::now().timestamp() as u64;

        // Simple signature: HMAC(associated_id + timestamp)
        let mut hasher = Sha256::new();
        hasher.update(secret);
        hasher.update(associated_id.as_bytes());
        hasher.update(timestamp.to_be_bytes());
        let signature = format!("{:x}", hasher.finalize());

        Ok(Self {
            associated_id,
            signature,
            timestamp,
        })
    }

    /// Validate authentication data
    pub fn validate(&self, secret: &[u8]) -> Result<bool, crate::WorkitError> {
        // Check if timestamp is expired
        let current_timestamp = chrono::Utc::now().timestamp() as u64;
        if current_timestamp.saturating_sub(self.timestamp) > 300 {
            return Ok(false);
        }

        // Validate signature
        let mut hasher = Sha256::new();
        hasher.update(secret);
        hasher.update(self.associated_id.as_bytes());
        hasher.update(self.timestamp.to_be_bytes());
        let expected_signature = format!("{:x}", hasher.finalize());

        Ok(self.signature == expected_signature)
    }
}
