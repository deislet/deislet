use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What a node proves its identity with when it opens its control channel.
///
/// The channel is gRPC, not a WebSocket - it was a WebSocket once and the
/// name stuck to these comments long after the transport changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthData {
    /// Associated ID (Node Instance ID)
    pub associated_id: String,
    /// Authentication signature (used only once at connection time)
    pub signature: String,
    /// Timestamp (to prevent auth data expiration)
    pub timestamp: u64,
}

impl AuthData {
    /// Build the authentication data, sent once when the gRPC control
    /// channel is established.
    pub fn new(associated_id: String, secret: &[u8]) -> Result<Self, crate::WireError> {
        let timestamp = chrono::Utc::now().timestamp() as u64;

        // sha256(secret || associated_id || timestamp). This is a
        // secret-prefix construction, not HMAC: it has no inner/outer pad, so
        // do not describe it as one. It is enough here because the input that
        // follows the secret is fixed-width and not attacker-extendable in a
        // way this protocol accepts, but a new use of it should reach for a
        // real MAC instead.
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
    pub fn validate(&self, secret: &[u8]) -> Result<bool, crate::WireError> {
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
