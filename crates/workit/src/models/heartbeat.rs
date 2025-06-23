use pwrzv::get_power_reserve_level_direct;
use serde::{Deserialize, Serialize};

/// Heartbeat request
/// send (id, power_reserve) to supervisor
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    /// Instance (associated) Id
    pub id: String,
    /// Power Reserve
    pub power_reserve: u8,
}

impl HeartbeatRequest {
    /// Create new heartbeat request
    pub async fn new(id: String) -> Self {
        if let Ok(power_reserve) = get_power_reserve_level_direct().await {
            return Self { id, power_reserve };
        }

        tracing::error!("Failed to get power reserve level");
        Self {
            id,
            power_reserve: 6, // default to 6 if failed to get power reserve level
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct HeartbeatResponse {
    pub success: bool,
    pub message: String,
}

impl HeartbeatResponse {
    pub fn new(success: bool, message: String) -> Self {
        Self { success, message }
    }
}
