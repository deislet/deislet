use deis_wire::{ControlConfig, NodeInfo, NodeRole, WireClient};

const TEST_SECRET: &str = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Wire → Control Integration Test ===\n");

    let config = ControlConfig::new(
        "test-deis_runtime-node".to_string(),
        TEST_SECRET.to_string(),
        "ws://127.0.0.1:9001".to_string(),
    );

    let node_info = NodeInfo::new_single_port(
        NodeRole::Runtime("1.0.0".to_string()),
        "127.0.0.1".to_string(),
        8000,
    );

    let mut client = WireClient::new(config, node_info);

    println!("[1] Connecting to deis_control...");
    client.connect().await?;
    println!("[OK] Connected\n");

    println!("[2] Authenticating...");
    client.authenticate().await?;
    println!("[OK] Authenticated\n");

    println!("[3] Registering node...");
    client.register().await?;
    println!("[OK] Registered\n");

    println!("[4] Sending heartbeat...");
    client.send_heartbeat().await?;
    println!("[OK] Heartbeat acknowledged\n");

    println!("=== All tests passed! ===");
    Ok(())
}
