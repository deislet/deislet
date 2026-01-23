// Resource type definitions

use serde::{Deserialize, Serialize};

/// Resource type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub name: String,
    pub resource_type: String,
}
