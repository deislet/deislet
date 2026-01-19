// 资源类型定义

use serde::{Deserialize, Serialize};

/// 资源类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub name: String,
    pub resource_type: String,
}
