//! 可扩展消息系统演示
//!
//! 展示如何使用统一的基于关联类型的消息系统

use serde_json::json;
use uuid::Uuid;
use workit::{
    WorkerInfo, WorkerType,
    models::{
        AuthData, HeartbeatRequest, RegisterRequest,
        messages::{
            AuthMessage, AuthResponseData, AuthResponseMessage, HeartbeatMessage, Message,
            MessageBuilder, RegisterMessage, WorkitMessage, deflux::PipelineStage,
        },
    },
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== WorkIt 统一消息系统演示 ===\n");

    // 1. 核心消息类型演示
    demonstrate_core_messages().await?;

    // 2. 扩展消息类型演示
    demonstrate_extension_messages().await?;

    // 3. 消息序列化演示
    demonstrate_message_serialization().await?;

    // 4. 请求-响应模式演示
    demonstrate_request_response_pattern().await?;

    println!("\n=== 演示完成 ===");
    Ok(())
}

/// 演示核心消息类型
async fn demonstrate_core_messages() -> Result<(), Box<dyn std::error::Error>> {
    println!("1. 核心消息类型演示");
    println!("{}", "-".repeat(40));

    // 认证消息
    let auth_data = AuthData::new("demo_worker".to_string(), b"demo_key")?;
    let auth_message = MessageBuilder::auth(auth_data);
    println!("认证消息: {}", serde_json::to_string_pretty(&auth_message)?);

    // 注册消息
    let worker_info = WorkerInfo::new(
        WorkerType::Denix("1.0".to_string()),
        "127.0.0.1".to_string(),
        vec![8080],
    );
    let register_data = RegisterRequest {
        id: "demo_worker".to_string(),
        info: worker_info,
    };
    let register_message = MessageBuilder::register(register_data);
    println!(
        "\n注册消息: {}",
        serde_json::to_string_pretty(&register_message)?
    );

    // 心跳消息
    let heartbeat_data = HeartbeatRequest {
        id: "demo_worker".to_string(),
        power_reserve: 85,
    };
    let heartbeat_message = MessageBuilder::heartbeat(heartbeat_data);
    println!(
        "\n心跳消息: {}",
        serde_json::to_string_pretty(&heartbeat_message)?
    );

    // 命令响应消息
    let command_id = Uuid::new_v4();
    let response_message = MessageBuilder::command_response(
        command_id,
        true,
        "任务执行成功".to_string(),
        Some(json!({
            "execution_time": "2.5s",
            "result": "success",
            "output": "Task completed successfully"
        })),
    );
    println!(
        "\n命令响应消息: {}",
        serde_json::to_string_pretty(&response_message)?
    );

    // 状态更新消息
    let status_message = MessageBuilder::status_update(
        "processing".to_string(),
        Some(json!({
            "current_task": "data_processing",
            "progress": 65,
            "eta": "5 minutes"
        })),
    );
    println!(
        "\n状态更新消息: {}",
        serde_json::to_string_pretty(&status_message)?
    );

    println!();
    Ok(())
}

/// 演示扩展消息类型
async fn demonstrate_extension_messages() -> Result<(), Box<dyn std::error::Error>> {
    println!("2. 扩展消息类型演示");
    println!("{}", "-".repeat(40));

    // Supervisor 批量操作消息
    let supervisor_message = MessageBuilder::supervisor_operation(
        "batch_deploy".to_string(),
        vec![
            "worker-001".to_string(),
            "worker-002".to_string(),
            "worker-003".to_string(),
        ],
        json!({
            "image": "myapp:v1.2.3",
            "replicas": 3,
            "strategy": "rolling_update"
        }),
        Some(2), // 最多并行执行 2 个
    );
    println!(
        "Supervisor 批量操作: {}",
        serde_json::to_string_pretty(&supervisor_message)?
    );

    // Denix 容器部署消息
    let denix_message = MessageBuilder::denix_deployment(
        "nginx:1.21".to_string(),
        3,
        "production".to_string(),
        "500m".to_string(), // 0.5 CPU
        "1Gi".to_string(),  // 1GB 内存
    );
    println!(
        "\nDenix 容器部署: {}",
        serde_json::to_string_pretty(&denix_message)?
    );

    // Deflux 数据管道消息
    let pipeline_stages = vec![
        PipelineStage {
            name: "extract".to_string(),
            stage_type: "data_source".to_string(),
            config: json!({
                "source_type": "database",
                "connection": "postgresql://localhost:5432/data",
                "query": "SELECT * FROM raw_data WHERE created_at > NOW() - INTERVAL '1 hour'"
            }),
        },
        PipelineStage {
            name: "transform".to_string(),
            stage_type: "processor".to_string(),
            config: json!({
                "transformations": [
                    {"type": "filter", "condition": "value > 0"},
                    {"type": "normalize", "method": "z_score"},
                    {"type": "aggregate", "group_by": "category", "function": "sum"}
                ]
            }),
        },
        PipelineStage {
            name: "load".to_string(),
            stage_type: "data_sink".to_string(),
            config: json!({
                "destination_type": "data_warehouse",
                "connection": "bigquery://project/dataset/table",
                "write_mode": "append"
            }),
        },
    ];

    let deflux_message = MessageBuilder::deflux_pipeline(
        "hourly_analytics".to_string(),
        "每小时数据分析管道".to_string(),
        pipeline_stages,
    );
    println!(
        "\nDeflux 数据管道: {}",
        serde_json::to_string_pretty(&deflux_message)?
    );

    println!();
    Ok(())
}

/// 演示消息序列化
async fn demonstrate_message_serialization() -> Result<(), Box<dyn std::error::Error>> {
    println!("3. 消息序列化演示");
    println!("{}", "-".repeat(40));

    // 创建一个复杂的消息
    let auth_data = AuthData::new("serialization_test".to_string(), b"test_key")?;
    let original_message = MessageBuilder::auth(auth_data);

    // 序列化
    let json_string = serde_json::to_string(&original_message)?;
    println!("序列化后的 JSON:");
    println!("{}", serde_json::to_string_pretty(&original_message)?);

    // 反序列化
    let deserialized_message: WorkitMessage<AuthMessage> = serde_json::from_str(&json_string)?;
    println!("\n反序列化成功!");
    println!("消息类型: {}", deserialized_message.message_type);
    println!("时间戳: {}", deserialized_message.timestamp);
    println!("Worker ID: {}", deserialized_message.data.associated_id);

    // 验证消息类型常量
    println!("\n消息类型常量验证:");
    println!("AuthMessage::MESSAGE_TYPE = {}", AuthMessage::MESSAGE_TYPE);
    println!(
        "RegisterMessage::MESSAGE_TYPE = {}",
        RegisterMessage::MESSAGE_TYPE
    );
    println!(
        "HeartbeatMessage::MESSAGE_TYPE = {}",
        HeartbeatMessage::MESSAGE_TYPE
    );

    println!();
    Ok(())
}

/// 演示请求-响应模式
async fn demonstrate_request_response_pattern() -> Result<(), Box<dyn std::error::Error>> {
    println!("4. 请求-响应模式演示");
    println!("{}", "-".repeat(40));

    // 创建请求
    let request_id = Uuid::new_v4();
    let auth_data = AuthData::new("request_response_test".to_string(), b"test_key")?;
    let auth_msg = AuthMessage::with_id(auth_data, request_id);
    let request = WorkitMessage::new(auth_msg);

    println!("请求消息:");
    println!("  消息 ID: {:?}", request.message_id);
    println!("  消息类型: {}", request.message_type);

    // 创建响应
    let response_data = AuthResponseData {
        authenticated: true,
        message: "认证成功".to_string(),
        error: None,
    };
    let auth_response = AuthResponseMessage::new(response_data);
    let response = WorkitMessage::response_to(auth_response, &request);

    println!("\n响应消息:");
    println!("  消息 ID: {:?}", response.message_id);
    println!("  关联 ID: {:?}", response.correlation_id);
    println!("  消息类型: {}", response.message_type);
    println!("  认证结果: {}", response.data.authenticated);

    // 验证关联关系
    assert_eq!(response.correlation_id, request.message_id);
    println!("\n✓ 请求-响应关联验证成功!");

    println!();
    Ok(())
}

/// 演示自定义消息类型扩展
#[allow(dead_code)]
async fn demonstrate_custom_extension() -> Result<(), Box<dyn std::error::Error>> {
    println!("5. 自定义消息类型扩展演示");
    println!("{}", "-".repeat(40));

    // 这里可以展示如何创建完全自定义的消息类型
    // 例如为新的系统（如监控系统）创建消息类型

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct MonitoringData {
        metric_name: String,
        value: f64,
        tags: std::collections::HashMap<String, String>,
    }

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct MonitoringMessage {
        monitoring_data: MonitoringData,
        message_id: Option<Uuid>,
    }

    impl Message for MonitoringMessage {
        const MESSAGE_TYPE: &'static str = "monitoring_metric";
        type Data = MonitoringData;

        fn data(&self) -> &Self::Data {
            &self.monitoring_data
        }

        fn message_id(&self) -> Option<Uuid> {
            self.message_id
        }

        fn new(data: Self::Data) -> Self {
            Self {
                monitoring_data: data,
                message_id: Some(Uuid::new_v4()),
            }
        }

        fn with_id(data: Self::Data, message_id: Uuid) -> Self {
            Self {
                monitoring_data: data,
                message_id: Some(message_id),
            }
        }
    }

    // 使用自定义消息类型
    let monitoring_data = MonitoringData {
        metric_name: "cpu_usage".to_string(),
        value: 85.5,
        tags: std::collections::HashMap::from([
            ("host".to_string(), "server-001".to_string()),
            ("region".to_string(), "us-west-2".to_string()),
        ]),
    };

    let monitoring_msg = MonitoringMessage::new(monitoring_data);
    let workit_msg = WorkitMessage::new(monitoring_msg);

    println!(
        "自定义监控消息: {}",
        serde_json::to_string_pretty(&workit_msg)?
    );
    println!("消息类型: {}", workit_msg.message_type);

    println!();
    Ok(())
}
