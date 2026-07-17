use denira::*;
use std::sync::Arc;
use tokio::time::Duration;

/// Poll the given TCP port until the API server accepts connections,
/// so tests do not rely on fixed delays for startup synchronization.
/// Panics if the server is not ready within the attempt limit.
async fn wait_for_server_ready(port: u16) {
    const MAX_ATTEMPTS: u32 = 100;
    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    // The first tick completes immediately, so the first probe has no delay.
    for _ in 0..MAX_ATTEMPTS {
        ticker.tick().await;
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
    }
    panic!("API server on port {port} did not become ready in time");
}

#[tokio::test]
async fn test_api_server_health() {
    // Create test configuration
    let config = SupervisorConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());

    // Create server components
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

    // Start the API server in the background
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

    // Wait until the server is ready
    wait_for_server_ready(18081).await;

    // Test the health endpoint
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

    wait_for_server_ready(18082).await;

    // Test the workers endpoint
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

    wait_for_server_ready(18083).await;

    // Test the metrics endpoint
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

    wait_for_server_ready(18084).await;

    // Test the events endpoint
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
