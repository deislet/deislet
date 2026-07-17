use denix::supervisor::{DenixSupervisorClient, DenixSupervisorConfig};

const TEST_SECRET: &str = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Denix Supervisor Integration Test ===\n");

    let config = DenixSupervisorConfig {
        addr: "ws://127.0.0.1:9001".to_string(),
        associated_id: "denix-test-worker".to_string(),
        secret: TEST_SECRET.to_string(),
        heartbeat_interval: 30,
        request_timeout: 10,
        retry_count: 3,
    };

    println!("[1] Creating supervisor client...");
    let mut client = DenixSupervisorClient::new(config, 8000)?;
    println!("[OK]\n");

    println!("[2] Starting client (connect + auth + register)...");
    client.start().await?;
    println!("[OK]\n");

    println!("[3] Checking status...");
    assert!(client.is_ready(), "Client should be ready");
    println!("[OK] Client is ready\n");

    println!("[4] Waiting for heartbeat...");
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    println!("[OK]\n");

    println!("[5] Disconnecting...");
    client.disconnect().await?;
    assert!(!client.is_connected(), "Client should be disconnected");
    println!("[OK]\n");

    println!("=== All tests passed! ===");
    Ok(())
}
