use serde::{Deserialize, Serialize};

pub mod instance_fsm {
    use yasm::*;

    define_state_machine! {
        name: InstanceFSM,
        states: { Pending, Active, Sunsetting, Terminated },
        inputs: { Registed, _Heartbeat, Shutdown, Silenced, Startup },
        initial: Pending,
        transitions: {
            Pending + Registed => Active,
            Active + _Heartbeat => Active,
            Active + Shutdown => Sunsetting,
            Sunsetting + _Heartbeat => Sunsetting,
            Sunsetting + Silenced => Terminated,
            Terminated + Startup => Pending,
        }
    }
}

// Re-export for easy access
pub use instance_fsm::State as InstanceStatus;

use crate::WorkerInfo;

/// Instance detailed information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceDetails {
    /// Instance ID
    pub id: String,
    /// Worker Info
    pub info: WorkerInfo,
    /// Current status
    pub status: InstanceStatus,
    /// Last heartbeat time
    pub last_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
}
