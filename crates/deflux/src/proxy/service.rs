//! Proxy service implementation
//!
//! This module provides the core proxy functionality using the new ConfigManager
//! with ArcSwap for high-performance configuration access.

use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use anyhow::Result;

use crate::config::ConfigManager;
use super::handler::ProxyHandler;

pub struct ProxyService {
    config_manager: Arc<ConfigManager>,
    handler: ProxyHandler,
}

impl ProxyService {
    pub fn new(config_manager: Arc<ConfigManager>) -> Self {
        let handler = ProxyHandler::new(Arc::clone(&config_manager));
        Self { config_manager, handler }
    }

    pub async fn handle_connection(&self, stream: TcpStream) -> Result<()> {
        self.handler.handle_connection(stream).await
    }

    /// Get configuration for a specific app using ArcSwap (lock-free)
    pub fn get_app_config(&self, app_id: &str) -> Option<JsonValue> {
        self.config_manager.get_app_config(app_id)
    }

    /// Find app configuration by domain name
    pub fn find_app_by_domain(&self, domain: &str) -> Option<(String, JsonValue)> {
        let configs = self.config_manager.get_app_configs_arc().load();
        
        for (app_id, config) in configs.iter() {
            if let Some(domains) = config.get("domains").and_then(|d| d.as_array()) {
                for domain_value in domains {
                    if let Some(config_domain) = domain_value.as_str()
                        && config_domain == domain
                    {
                        return Some((app_id.clone(), config.clone()));
                    }
                }
            }
        }
        None
    }

    /// Check if an app is enabled
    pub fn is_app_enabled(&self, app_id: &str) -> bool {
        if let Some(config) = self.get_app_config(app_id) {
            config.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false)
        } else {
            false
        }
    }

    /// Get all enabled apps
    pub fn get_enabled_apps(&self) -> Vec<(String, JsonValue)> {
        let configs = self.config_manager.get_app_configs_arc().load();
        
        configs
            .iter()
            .filter(|(_, config)| {
                config.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false)
            })
            .map(|(app_id, config)| (app_id.clone(), config.clone()))
            .collect()
    }

    /// Get configuration statistics
    pub fn get_config_stats(&self) -> HashMap<String, serde_json::Value> {
        let mut stats = HashMap::new();
        stats.insert("total_apps".to_string(), self.config_manager.get_app_count().into());
        
        let enabled_count = self.get_enabled_apps().len();
        stats.insert("enabled_apps".to_string(), enabled_count.into());
        
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigDatabase, ConfigManager};
    use chrono::Utc;
    use serde_json::json;
    use tempfile::NamedTempFile;
    use std::path::PathBuf;

    async fn create_test_service() -> ProxyService {
        let temp_db = NamedTempFile::new().unwrap();
        let temp_config = NamedTempFile::new().unwrap();
        
        // Create complete config file with all required fields
        let config_content = r#"
[supervisor_client]
address = "ws://test:8080/ws"
instance_id = "test-instance"
secret = "test-secret"

[[upstreams]]
host = "127.0.0.1"
port = 8001
"#;
        std::fs::write(temp_config.path(), config_content).unwrap();
        
        // Create database with a proper temp file path
        let db_path = temp_db.path().to_path_buf();
        drop(temp_db); // Close file so SQLite can open it
        
        let db = ConfigDatabase::init(&db_path).await.unwrap();
        let config_manager = ConfigManager::new(
            PathBuf::from(temp_config.path()),
            db
        ).await.unwrap();
        
        ProxyService::new(Arc::new(config_manager))
    }

    #[tokio::test]
    async fn test_proxy_service_creation() {
        let service = create_test_service().await;
        
        // Should start with no apps
        assert_eq!(service.config_manager.get_app_count(), 0);
        let stats = service.get_config_stats();
        assert_eq!(stats["total_apps"], 0);
        assert_eq!(stats["enabled_apps"], 0);
    }

    #[tokio::test]
    async fn test_app_config_access() {
        let service = create_test_service().await;
        
        // Add test app config
        let updates = vec![
            crate::config::manager::database::AppConfigEntry {
                app_id: "app1".to_string(),
                config: json!({
                    "id": "app1",
                    "domains": ["example.com", "www.example.com"],
                    "enabled": true,
                    "upstream": "http://backend:8080"
                }),
                version_timestamp: Utc::now(),
            }
        ];
        
        service.config_manager.update_app_configs(updates).await.unwrap();
        
        // Test config retrieval
        let config = service.get_app_config("app1").unwrap();
        assert_eq!(config["id"], "app1");
        assert_eq!(config["enabled"], true);
        
        // Test domain lookup
        let (app_id, app_config) = service.find_app_by_domain("example.com").unwrap();
        assert_eq!(app_id, "app1");
        assert_eq!(app_config["upstream"], "http://backend:8080");
        
        // Test enabled check
        assert!(service.is_app_enabled("app1"));
        assert!(!service.is_app_enabled("nonexistent"));
    }

    #[tokio::test]
    async fn test_enabled_apps_filtering() {
        let service = create_test_service().await;
        
        let updates = vec![
            crate::config::manager::database::AppConfigEntry {
                app_id: "app1".to_string(),
                config: json!({
                    "id": "app1",
                    "domains": ["example.com"],
                    "enabled": true
                }),
                version_timestamp: Utc::now(),
            },
            crate::config::manager::database::AppConfigEntry {
                app_id: "app2".to_string(),
                config: json!({
                    "id": "app2", 
                    "domains": ["test.com"],
                    "enabled": false
                }),
                version_timestamp: Utc::now(),
            },
            crate::config::manager::database::AppConfigEntry {
                app_id: "app3".to_string(),
                config: json!({
                    "id": "app3",
                    "domains": ["active.com"],
                    "enabled": true
                }),
                version_timestamp: Utc::now(),
            }
        ];
        
        service.config_manager.update_app_configs(updates).await.unwrap();
        
        // Test enabled apps filtering
        let enabled_apps = service.get_enabled_apps();
        assert_eq!(enabled_apps.len(), 2);
        
        let enabled_ids: Vec<String> = enabled_apps.iter().map(|(id, _)| id.clone()).collect();
        assert!(enabled_ids.contains(&"app1".to_string()));
        assert!(enabled_ids.contains(&"app3".to_string()));
        assert!(!enabled_ids.contains(&"app2".to_string()));
        
        // Test stats
        let stats = service.get_config_stats();
        assert_eq!(stats["total_apps"], 3);
        assert_eq!(stats["enabled_apps"], 2);
    }

    #[tokio::test]
    async fn test_domain_lookup() {
        let service = create_test_service().await;
        
        let updates = vec![
            crate::config::manager::database::AppConfigEntry {
                app_id: "multi-domain-app".to_string(),
                config: json!({
                    "id": "multi-domain-app",
                    "domains": ["example.com", "www.example.com", "api.example.com"],
                    "enabled": true,
                    "backend": "app-backend:3000"
                }),
                version_timestamp: Utc::now(),
            }
        ];
        
        service.config_manager.update_app_configs(updates).await.unwrap();
        
        // Test all domain variations
        for domain in &["example.com", "www.example.com", "api.example.com"] {
            let result = service.find_app_by_domain(domain);
            assert!(result.is_some(), "Domain {} should be found", domain);
            
            let (app_id, config) = result.unwrap();
            assert_eq!(app_id, "multi-domain-app");
            assert_eq!(config["backend"], "app-backend:3000");
        }
        
        // Test non-existent domain
        assert!(service.find_app_by_domain("notfound.com").is_none());
    }

    #[tokio::test]
    async fn test_arcswap_direct_access() {
        let service = create_test_service().await;
        
        let updates = vec![
            crate::config::manager::database::AppConfigEntry {
                app_id: "test-app".to_string(),
                config: json!({
                    "id": "test-app",
                    "enabled": true,
                    "custom_field": "custom_value"
                }),
                version_timestamp: Utc::now(),
            }
        ];
        
        service.config_manager.update_app_configs(updates).await.unwrap();
        
        // Test direct ArcSwap access
        let app_configs_arc = service.config_manager.get_app_configs_arc();
        let configs = app_configs_arc.load();
        
        assert_eq!(configs.len(), 1);
        assert!(configs.contains_key("test-app"));
        assert_eq!(configs["test-app"]["custom_field"], "custom_value");
    }

    #[tokio::test]
    async fn test_config_stats() {
        let service = create_test_service().await;
        
        // Initially empty
        let stats = service.get_config_stats();
        assert_eq!(stats["total_apps"], 0);
        assert_eq!(stats["enabled_apps"], 0);
        
        // Add mixed enabled/disabled apps
        let updates = vec![
            crate::config::manager::database::AppConfigEntry {
                app_id: "enabled1".to_string(),
                config: json!({"enabled": true}),
                version_timestamp: Utc::now(),
            },
            crate::config::manager::database::AppConfigEntry {
                app_id: "disabled1".to_string(),
                config: json!({"enabled": false}),
                version_timestamp: Utc::now(),
            },
            crate::config::manager::database::AppConfigEntry {
                app_id: "enabled2".to_string(),
                config: json!({"enabled": true}),
                version_timestamp: Utc::now(),
            }
        ];
        
        service.config_manager.update_app_configs(updates).await.unwrap();
        
        let stats = service.get_config_stats();
        assert_eq!(stats["total_apps"], 3);
        assert_eq!(stats["enabled_apps"], 2);
    }
} 