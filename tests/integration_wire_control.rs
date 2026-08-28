use deis_wire::{SupervisorConfig, WorkerInfo, WorkerType, WireClient};

const TEST_SECRET: &str = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Wire → Control Integration Test ===\n");

    let config = SupervisorConfig::new(
        "test-deis_runtime-worker".to_string(),
        TEST_SECRET.to_string(),
        "ws://127.0.0.1:9001".to_string(),
    );

    let worker_info = WorkerInfo::new_single_port(
        WorkerType::Runtime("1.0.0".to_string()),
        "127.0.0.1".to_string(),
        8000,
    );

    let mut client = WireClient::new(config, worker_info);

    println!("[1] Connecting to deis_control...");
    client.connect().await?;
    println!("[OK] Connected\n");

    println!("[2] Authenticating...");
    client.authenticate().await?;
    println!("[OK] Authenticated\n");

    println!("[3] Registering worker...");
    client.register().await?;
    println!("[OK] Registered\n");

    println!("[4] Sending heartbeat...");
    client.send_heartbeat().await?;
    println!("[OK] Heartbeat acknowledged\n");

    println!("=== All tests passed! ===");
    Ok(())
}
