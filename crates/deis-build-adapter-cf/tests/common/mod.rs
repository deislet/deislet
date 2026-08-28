use deis_build_core::{ProjectConfig, ServicesConfig, KVConfig};

pub fn test_config() -> ProjectConfig {
    ProjectConfig {
        version: "1.0.0".to_string(),
        name: "test-worker".to_string(),
        runtime: "cloudflare".to_string(),
        language: "typescript".to_string(),
        function_root: "./functions".to_string(),
        routing: None,
        environment: None,
        services: None,
        vendors: None,
        build: None,
        limits: None,
    }
}

pub fn test_config_with_kv() -> ProjectConfig {
    let mut config = test_config();
    config.services = Some(ServicesConfig {
        kv: Some(KVConfig { enabled: true }),
        database: None,
    });
    config
}
