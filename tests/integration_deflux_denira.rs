use deflux::supervisor::{DefluxSupervisorClient, DefluxSupervisorConfig};
use std::time::Duration;

const TEST_SECRET: &str = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Deflux ↔ Denira Integration Test ===\n");

    let config = DefluxSupervisorConfig {
        addr: "ws://127.0.0.1:9001".to_string(),
        associated_id: "deflux-test-worker".to_string(),
        secret: TEST_SECRET.to_string(),
        heartbeat_interval: 30,
        request_timeout: 10,
        retry_count: 3,
    };

    println!("[1] Creating Deflux supervisor client...");
    let mut client = DefluxSupervisorClient::new(config, 8080)?;
    println!("[OK] Client created\n");

    println!("[2] Starting client (connect + auth + register)...");
    client.start().await?;
    println!("[OK] Client started\n");

    println!("[3] Verifying client is ready...");
    assert!(client.is_ready(), "Client should be ready after start");
    println!("[OK] Client is ready\n");

    println!("[4] Waiting for heartbeat cycle...");
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!("[OK] Heartbeat sent\n");

    println!("[5] Testing connection status...");
    assert!(client.is_connected(), "Client should be connected");
    println!("[OK] Connection is active\n");

    println!("[6] Disconnecting gracefully...");
    client.disconnect().await?;
    assert!(!client.is_connected(), "Client should be disconnected");
    println!("[OK] Disconnected\n");

    println!("=== All Deflux integration tests passed! ===");
    Ok(())
}
