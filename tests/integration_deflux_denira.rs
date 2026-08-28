use deflux::supervisor::DefluxSupervisorClient;
use denira_workit::SupervisorConfig;

const TEST_SECRET: &str = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Deflux <-> Denira Integration Test ===\n");

    let config = SupervisorConfig::new(
        "deflux-test-worker".to_string(),
        TEST_SECRET.to_string(),
        "ws://127.0.0.1:9001".to_string(),
    );

    println!("[1] Creating Deflux supervisor client...");
    let mut client = DefluxSupervisorClient::new(config)?;
    println!("[OK] Client created\n");

    println!("[2] Starting client (connect + auth + register)...");
    client.start().await?;
    println!("[OK] Client started\n");

    println!("[3] Verifying client is ready...");
    assert!(client.is_ready().await, "client should be ready after start");
    println!("[OK] Client is ready\n");

    println!("[4] Disconnecting gracefully...");
    client.disconnect().await?;
    assert!(
        !client.is_ready().await,
        "client should not be ready after disconnect"
    );
    println!("[OK] Disconnected\n");

    println!("=== All Deflux integration tests passed! ===");
    Ok(())
}
