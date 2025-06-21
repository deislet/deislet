//! Workit SDK 基本使用示例
//!
//! 此示例展示了如何使用 Workit SDK 来实现 worker server 与 supervisor 的通信

// use std::sync::atomic::{AtomicU32, Ordering};
// use std::sync::Arc;
use tokio::time::{Duration, sleep};
use tracing::{Level, info};
use workit::{SdkResult, SupervisorConfig, WorkerInfo, WorkerType, Workit, WorkitError};

#[tokio::main]
async fn main() -> SdkResult<()> {
    // 初始化日志
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    // 配置 Supervisor 连接信息
    let supervisor_config = SupervisorConfig::new(
        "worker-001".to_string(),
        "a1b2c3d4e5f6789012345678901234567890123456789012345678901234567890".to_string(),
        "http://localhost:8080".to_string(),
    );

    // 配置 Worker 信息, 包括 Worker 类型和版本
    let worker_info = WorkerInfo::new_single_port(
        WorkerType::Denix("0.1.0-dev".to_string()),
        "192.168.1.100".to_string(),
        3000,
    );

    // 创建 SDK 客户端
    let client = Workit::new(supervisor_config, worker_info)?;

    info!("启动 Denira SDK 服务...");

    // 方法 1: 先注册，然后启动心跳任务
    let register_response = client.register().await?;
    info!("注册成功: {}", register_response.message);

    // 启动心跳任务
    let heartbeat_handle = client.start_heartbeat_task().await;

    // 主程序运行一段时间后停止
    info!("服务运行中，60 秒后停止...");
    sleep(Duration::from_secs(60)).await;

    // 停止心跳任务
    heartbeat_handle.abort();
    info!("服务已停止");

    Ok(())
}

/// 分步骤使用示例
#[allow(dead_code)]
async fn step_by_step_example() -> SdkResult<()> {
    // 配置信息
    let supervisor_config = SupervisorConfig::new(
        "worker-002".to_string(),
        "b2c3d4e5f6789012345678901234567890123456789012345678901234567890".to_string(),
        "http://localhost:8080".to_string(),
    );

    // 使用 Deflux (LB) worker 类型
    let worker_info = WorkerInfo::new_single_port(
        WorkerType::Deflux("1.2.0".to_string()),
        "192.168.1.101".to_string(),
        3001,
    );

    // 创建客户端
    let client = Workit::new(supervisor_config, worker_info)?;

    // 步骤 1: 注册到 Supervisor
    info!("步骤 1: 注册到 Supervisor");
    let register_response = client.register().await?;
    info!("注册响应: {}", register_response.message);

    // 步骤 2: 发送单次心跳
    info!("步骤 2: 发送单次心跳");
    let heartbeat_response = client.heartbeat().await?;
    info!("心跳响应: {}", heartbeat_response.message);

    // 步骤 3: 查看状态
    info!("步骤 3: 查看当前状态");
    let status = client.get_status().await;
    info!("当前状态: {:?}", status);

    // 步骤 4: 获取实例详细信息
    info!("步骤 4: 获取实例详细信息");
    let details = client.get_instance_details().await;
    info!("实例详细信息: {:#?}", details);

    Ok(())
}

/// 错误处理示例
#[allow(dead_code)]
async fn error_handling_example() -> SdkResult<()> {
    // 使用错误的配置来演示错误处理
    let supervisor_config = SupervisorConfig::new(
        "".to_string(),               // 空的 ID 会导致配置错误
        "invalid_secret".to_string(), // 无效的密钥格式
        "invalid_url".to_string(),    // 无效的 URL
    );

    let worker_info = WorkerInfo::new_single_port(
        WorkerType::Denix("1.0.0".to_string()),
        "localhost".to_string(),
        3000,
    );

    // 尝试创建客户端（这会失败）
    match Workit::new(supervisor_config, worker_info) {
        Ok(_) => info!("客户端创建成功"),
        Err(e) => {
            info!("客户端创建失败（这是预期的）: {}", e);

            // 根据错误类型进行不同的处理
            match e {
                WorkitError::ConfigurationError(msg) => {
                    info!("配置错误: {}", msg);
                }
                WorkitError::AuthenticationError(msg) => {
                    info!("认证错误: {}", msg);
                }
                WorkitError::NetworkError(msg) => {
                    info!("网络错误: {}", msg);
                }
                _ => {
                    info!("其他错误: {}", e);
                }
            }
        }
    }

    Ok(())
}
