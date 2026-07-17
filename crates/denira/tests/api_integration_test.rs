use denira::*;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_api_server_health() {
    // 创建测试配置
    let config = SupervisorConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());

    // 创建服务器组件
    let sessions = Arc::new(SessionManager::new());
    let broadcaster = Arc::new(EventBroadcaster::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    // 在后台启动 API 服务器
    let api_config = Arc::new(config.clone());
    tokio::spawn(async move {
        let _ = denira::api::start_api_server(
            sessions,
            database,
            broadcaster,
            config_manager,
            command_executor,
            metrics_collector,
            api_config,
            18081,
        )
        .await;
    });

    // 等待服务器启动
    sleep(Duration::from_secs(1)).await;

    // 测试 health 端点
    let client = reqwest::Client::new();
    let response = client
        .get("http://localhost:18081/health")
        .send()
        .await
        .expect("Failed to call health endpoint");

    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert_eq!(body, "OK");
}

#[tokio::test]
async fn test_api_workers_endpoint() {
    let config = SupervisorConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let broadcaster = Arc::new(EventBroadcaster::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    let api_config = Arc::new(config.clone());
    tokio::spawn(async move {
        let _ = denira::api::start_api_server(
            sessions,
            database,
            broadcaster,
            config_manager,
            command_executor,
            metrics_collector,
            api_config,
            18082,
        )
        .await;
    });

    sleep(Duration::from_secs(1)).await;

    // 测试 workers 端点
    let client = reqwest::Client::new();
    let response = client
        .get("http://localhost:18082/api/workers")
        .send()
        .await
        .expect("Failed to call workers endpoint");

    assert_eq!(response.status(), 200);

    let json: serde_json::Value = response.json().await.unwrap();
    assert!(json.get("workers").is_some());
    assert!(json.get("total").is_some());
}

#[tokio::test]
async fn test_api_metrics_endpoint() {
    let config = SupervisorConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let broadcaster = Arc::new(EventBroadcaster::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    let api_config = Arc::new(config.clone());
    tokio::spawn(async move {
        let _ = denira::api::start_api_server(
            sessions,
            database,
            broadcaster,
            config_manager,
            command_executor,
            metrics_collector,
            api_config,
            18083,
        )
        .await;
    });

    sleep(Duration::from_secs(1)).await;

    // 测试 metrics 端点
    let client = reqwest::Client::new();
    let response = client
        .get("http://localhost:18083/api/metrics/summary")
        .send()
        .await
        .expect("Failed to call metrics endpoint");

    assert_eq!(response.status(), 200);

    let json: serde_json::Value = response.json().await.unwrap();
    assert!(json.get("metrics").is_some());
    assert!(json["metrics"].get("workers").is_some());
    assert!(json["metrics"].get("system").is_some());
}

#[tokio::test]
async fn test_api_events_endpoint() {
    let config = SupervisorConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let broadcaster = Arc::new(EventBroadcaster::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    let api_config = Arc::new(config.clone());
    tokio::spawn(async move {
        let _ = denira::api::start_api_server(
            sessions,
            database,
            broadcaster,
            config_manager,
            command_executor,
            metrics_collector,
            api_config,
            18084,
        )
        .await;
    });

    sleep(Duration::from_secs(1)).await;

    // 测试 events 端点
    let client = reqwest::Client::new();
    let response = client
        .get("http://localhost:18084/api/events")
        .send()
        .await
        .expect("Failed to call events endpoint");

    assert_eq!(response.status(), 200);

    let json: serde_json::Value = response.json().await.unwrap();
    assert!(json.get("events").is_some());
}
