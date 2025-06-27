//! Proxy module (simplified)
//!
//! Main proxy server implementation using new configuration management system

pub mod admin;
pub mod balancer;
pub mod service;

use anyhow::Result;
use arc_swap::ArcSwap;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;

/// Main Deflux server using simplified configuration
pub struct DefluxServer {
    /// Direct access to app configurations via ArcSwap
    app_configs: Arc<ArcSwap<HashMap<String, JsonValue>>>,
}

impl DefluxServer {
    /// Create a new Deflux server
    pub fn new(app_configs: Arc<ArcSwap<HashMap<String, JsonValue>>>) -> Result<Self> {
        Ok(Self { app_configs })
    }

    /// Start the server (placeholder)
    pub async fn start(&self) -> Result<()> {
        // TODO: Implement actual server startup with Pingora
        println!("Deflux server starting with {} apps configured", 
                 self.app_configs.load().len());
        Ok(())
    }

    /// Get server statistics
    pub fn get_stats(&self) -> JsonValue {
        let configs = self.app_configs.load();
        
        serde_json::json!({
            "status": "running",
            "total_apps": configs.len(),
            "enabled_apps": configs.values().filter(|config| {
                config.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false)
            }).count()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_server_creation() -> Result<()> {
        let app_configs = Arc::new(ArcSwap::new(Arc::new(HashMap::new())));
        let _server = DefluxServer::new(app_configs)?;
        Ok(())
    }

    #[test]
    fn test_server_stats() {
        let mut configs = HashMap::new();
        configs.insert("app1".to_string(), json!({"id": "app1", "enabled": true}));
        configs.insert("app2".to_string(), json!({"id": "app2", "enabled": false}));
        
        let app_configs = Arc::new(ArcSwap::new(Arc::new(configs)));
        let server = DefluxServer::new(app_configs).unwrap();
        
        let stats = server.get_stats();
        assert_eq!(stats["total_apps"], 2);
        assert_eq!(stats["enabled_apps"], 1);
        assert_eq!(stats["status"], "running");
    }
}
