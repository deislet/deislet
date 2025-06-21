# Workit

Workit 是一个用于 Denira supervisor 和 Worker server 之间通信的 Rust 库。它提供了完整的认证、注册、心跳等功能，简化了 Worker server 与 supervisor 的集成过程。

## 特性

- **安全认证**: 基于 PSK + Nonce 的 HMAC-SHA256 签名认证
- **自动注册**: 简化的 worker 实例注册流程
- **心跳管理**: 自动心跳发送和状态监控
- **错误处理**: 完善的错误类型和重试机制
- **配置验证**: 自动验证配置参数的有效性
- **异步支持**: 基于 tokio 的异步编程支持

## 快速开始

### 添加依赖

在你的 `Cargo.toml` 中添加：

```toml
[dependencies]
workit = { path = "../denira/crates/workit" }
tokio = { version = "1.0", features = ["full"] }
tracing = "0.1"
```

### 基本使用

```rust
use denira_sdk::{DeniraClient, SupervisorConfig, WorkerInfo, SystemStatus};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 配置 Supervisor 连接
    let supervisor_config = SupervisorConfig::new(
        "worker-001".to_string(),
        "your-64-char-hex-secret".to_string(),
        "http://supervisor.example.com:8080".to_string(),
    );

    // 配置 Worker 信息
    let worker_info = WorkerInfo::new(
        "worker-001".to_string(),
        "192.168.1.100".to_string(),
        "1.0.0".to_string(),
        "192.168.1.100:3000".to_string(),
    );

    // 创建客户端
    let client = DeniraClient::new(supervisor_config, worker_info)?;

    // 创建系统状态提供器
    let system_status_provider = || {
        SystemStatus::new(
            "15%".to_string(),    // CPU 使用率
            "256MB".to_string(),  // 内存使用量
            42,                   // 请求计数
        )
    };

    // 启动完整服务（注册 + 心跳）
    let handle = client.start_full_service(system_status_provider).await?;

    // 服务运行...
    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

    // 停止服务
    handle.abort();
    
    Ok(())
}
```

## 详细用法

### 分步骤操作

如果你需要更细粒度的控制，可以分步骤执行：

```rust
// 1. 注册到 Supervisor
let register_response = client.register().await?;
println!("注册成功: {}", register_response.message);

// 2. 发送单次心跳
let system_status = SystemStatus::new("10%".to_string(), "128MB".to_string(), 100);
let heartbeat_response = client.send_heartbeat(&system_status).await?;
println!("心跳发送成功: {}", heartbeat_response.message);

// 3. 启动自动心跳任务
let heartbeat_handle = client.start_heartbeat_task(|| {
    // 返回当前系统状态
    get_current_system_status()
}).await?;
```

### 配置说明

#### SupervisorConfig

```rust
let config = SupervisorConfig {
    associated_id: "worker-001".to_string(),        // 实例关联 ID
    secret: "64-char-hex-secret".to_string(),       // 预共享密钥（64 字符十六进制）
    addr: "http://supervisor:8080".to_string(),     // Supervisor 地址
    heartbeat_interval: 30,                         // 心跳间隔（秒）
    request_timeout: 10,                            // 请求超时（秒）
    retry_count: 3,                                 // 重试次数
};
```

#### WorkerInfo

```rust
let worker_info = WorkerInfo {
    id: "worker-001".to_string(),                   // Worker 实例 ID
    host: "192.168.1.100".to_string(),             // 主机地址
    version: "1.0.0".to_string(),                  // 版本号
    listen_addr: "192.168.1.100:3000".to_string(), // 监听地址
};
```

### 错误处理

工具包 提供了详细的错误类型：

```rust
use denira_sdk::WorkitError;

match client.register().await {
    Ok(response) => println!("注册成功: {}", response.message),
    Err(WorkitError::AuthenticationError(msg)) => {
        eprintln!("认证失败: {}", msg);
    }
    Err(WorkitError::NetworkError(msg)) => {
        eprintln!("网络错误: {}", msg);
    }
    Err(WorkitError::SupervisorError { code, message, help }) => {
        eprintln!("Supervisor 错误 [{}]: {}", code, message);
        if let Some(help_text) = help {
            eprintln!("帮助: {}", help_text);
        }
    }
    Err(e) => eprintln!("其他错误: {}", e),
}
```

### 状态监控

```rust
// 获取当前状态
let status = client.get_status().await;
println!("当前状态: {}", status);

// 获取详细信息
let details = client.get_instance_details().await;
println!("实例详细信息: {:#?}", details);

// 获取请求计数
let count = client.get_request_count().await;
println!("处理的请求数: {}", count);
```

## API 参考

### 主要类型

- `DeniraClient`: 主要的 工具包 客户端
- `SupervisorConfig`: Supervisor 连接配置
- `WorkerInfo`: Worker 实例信息
- `SystemStatus`: 系统状态信息
- `WorkitError`: 工具包 错误类型

### 主要方法

- `DeniraClient::new()`: 创建客户端
- `register()`: 注册到 Supervisor
- `send_heartbeat()`: 发送心跳
- `start_heartbeat_task()`: 启动心跳任务
- `start_full_service()`: 启动完整服务

## 认证机制

工具包 使用基于 PSK (Pre-Shared Key) + Nonce 的 HMAC-SHA256 签名认证：

1. **预注册**: 在 Supervisor 中预先创建实例并获取 `associated_id` 和 `secret`
2. **签名生成**: 使用 nonce-auth 库生成带时间戳和随机数的签名
3. **防重放**: 通过时间窗口验证和 nonce 唯一性防止重放攻击

### 密钥要求

- 长度: 必须是 64 个字符
- 格式: 十六进制字符串 (0-9, a-f)
- 示例: `a1b2c3d4e5f67890123456789012345678901234567890123456789012345678`

## 集成示例

### 与 Axum 集成

```rust
use axum::{Router, response::Json};
use denira_sdk::DeniraClient;
use std::sync::Arc;

#[derive(Clone)]
struct AppState {
    denira_client: Arc<DeniraClient>,
}

async fn health_check(state: axum::extract::State<AppState>) -> Json<serde_json::Value> {
    let status = state.denira_client.get_status().await;
    Json(serde_json::json!({
        "status": status.to_string(),
        "requests": state.denira_client.get_request_count().await
    }))
}

#[tokio::main]
async fn main() {
    // 创建 Denira 客户端
    let denira_client = Arc::new(DeniraClient::new(config, worker_info).unwrap());
    
    // 启动 Denira 服务
    denira_client.start_full_service(system_status_provider).await.unwrap();
    
    // 创建 Axum 应用
    let app = Router::new()
        .route("/health", axum::routing::get(health_check))
        .with_state(AppState { denira_client });
    
    // 启动服务器...
}
```

## 故障排除

### 常见错误

1. **密钥格式错误**
   - 确保密钥是 64 字符的十六进制字符串
   - 检查是否包含非法字符

2. **时间同步问题**
   - 确保 worker 和 supervisor 的系统时间同步
   - 时间差不应超过 1 分钟

3. **网络连接问题**
   - 检查 supervisor 地址是否正确
   - 确保网络连通性和防火墙设置

4. **实例未预注册**
   - 在 supervisor 中预先创建实例记录
   - 确保 associated_id 和 secret 匹配

### 调试日志

启用详细日志来调试问题：

```rust
tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .init();
```

## 许可证

此项目使用与 Denira 主项目相同的许可证。 