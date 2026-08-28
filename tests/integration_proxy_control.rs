use std::time::Duration;

use deis_proxy::control::ProxyControlClient;
use deis_wire::ControlConfig;

const TEST_SECRET: &str = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Proxy <-> Control Integration Test ===\n");

    let config = ControlConfig::new(
        "deis-proxy-test-node".to_string(),
        TEST_SECRET.to_string(),
        "ws://127.0.0.1:9001".to_string(),
    );

    println!("[1] Creating Proxy control client...");
    let mut client = ProxyControlClient::new(config)?;
    println!("[OK] Client created\n");

    println!("[2] Connecting, authenticating and registering (retries until deis_control answers)...");
    tokio::time::timeout(CONNECT_TIMEOUT, client.connect_with_backoff())
        .await
        .map_err(|_| {
            format!("deis_control did not accept the registration within {CONNECT_TIMEOUT:?}")
        })?;
    println!("[OK] Registered\n");

    println!("[3] Verifying client is ready...");
    assert!(
        client.is_ready().await,
        "client should be ready after registration"
    );
    println!("[OK] Client is ready\n");

    println!("[4] Disconnecting gracefully...");
    client.disconnect().await?;
    assert!(
        !client.is_ready().await,
        "client should not be ready after disconnect"
    );
    println!("[OK] Disconnected\n");

    println!("=== All Proxy integration tests passed! ===");
    Ok(())
}
