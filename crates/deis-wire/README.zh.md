# Wire

[![Crates.io](https://img.shields.io/crates/v/deis-wire.svg)](https://crates.io/crates/deis-wire)
[![Documentation](https://docs.rs/deis-wire/badge.svg)](https://docs.rs/deis-wire)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Wire 是一个用于 Control control 和 Node 服务器（Runtime/Proxy）之间基于 WebSocket 通信的 Rust SDK。它提供安全、实时的双向通信，具有**可扩展消息系统**、认证、注册、心跳监控和命令执行功能。

## 特性

- 🚀 **WebSocket 通信**: 通过 WebSocket 进行实时双向通信
- 🔐 **安全认证**: 基于连接时 PSK 认证和 HMAC-SHA256 签名
- 💓 **心跳监控**: 自动心跳发送和连接监控
- 📡 **命令处理**: 来自 control 的命令处理
- 🎯 **类型安全**: 强类型可扩展消息系统，具有编译时验证
- ⚡ **异步支持**: 基于 Tokio 的完全异步 API
- 🔧 **可扩展**: 支持自定义消息类型和扩展

## 消息系统架构

Wire 基于**统一消息系统**构建，允许类型安全、可扩展的通信：

### 核心消息类型

- **AuthMessage**: 与 control 的认证
- **RegisterMessage**: Node 注册
- **HeartbeatMessage**: 定期健康检查
- **CommandMessage**: 命令执行请求
- **CommandResponseMessage**: 命令执行结果
- **StatusUpdateMessage**: Node 状态更新

### 扩展消息类型

- **ControlMessage**: 批量操作和管理
- **RuntimeMessage**: 容器部署和编排
- **ProxyMessage**: 数据管道管理

### 自定义消息类型

您可以通过实现 `Message` trait 轻松创建自定义消息类型：

```rust
use deis-wire::models::messages::{Message, WireMessage};
use serde::{Serialize, Deserialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomData {
    pub field1: String,
    pub field2: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomMessage {
    pub data: CustomData,
    pub message_id: Option<Uuid>,
}

impl Message for CustomMessage {
    const MESSAGE_TYPE: &'static str = "custom_message";
    type Data = CustomData;

    fn data(&self) -> &Self::Data {
        &self.data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn new(data: Self::Data) -> Self {
        Self {
            data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            data,
            message_id: Some(message_id),
        }
    }
}
```

## 安装

将以下内容添加到您的 `Cargo.toml`:

```toml
[dependencies]
deis-wire = "0.1.0"
```

## 快速开始

### 基本使用

```rust
use deis-wire::{WireClient, ControlConfig, NodeInfo, NodeRole};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 配置 control 连接信息
    let config = ControlConfig::new(
        "node-001".to_string(),
        "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
        "ws://localhost:8080".to_string(),
    );
    
    // 配置 node 信息
    let node_info = NodeInfo::new(
        NodeRole::Runtime("1.0.0".to_string()),
        "127.0.0.1".to_string(),
        vec![3000],
    );
    
    // 创建客户端
    let mut client = WireClient::new(config, node_info);
    
    // 建立连接
    client.connect().await?;
    
    // 与 control 认证
    client.authenticate().await?;
    
    // 注册 node
    client.register().await?;
    
    // 发送心跳
    client.send_heartbeat().await?;
    
    // 运行主客户端循环（处理心跳和传入消息）
    client.run().await?;
    
    Ok(())
}
```

### 使用消息构建器

```rust
use deis-wire::models::{AuthData, HeartbeatRequest, RegisterRequest};
use deis-wire::models::messages::{MessageBuilder, WireMessage};

// 创建认证消息
let auth_data = AuthData::new("node-001".to_string(), b"secret_key")?;
let auth_message = MessageBuilder::auth(auth_data);

// 创建注册消息
let register_data = RegisterRequest {
    id: "node-001".to_string(),
    info: node_info,
};
let register_message = MessageBuilder::register(register_data);

// 创建心跳消息
let heartbeat_data = HeartbeatRequest {
    id: "node-001".to_string(),
    power_reserve: 85,
};
let heartbeat_message = MessageBuilder::heartbeat(heartbeat_data);
```

### 扩展消息

```rust
use deis-wire::models::messages::MessageBuilder;
use serde_json::json;

// Control 批量操作
let control_message = MessageBuilder::control_operation(
    "batch_deploy".to_string(),
    vec!["node-001".to_string(), "node-002".to_string()],
    json!({
        "image": "myapp:v1.2.3",
        "replicas": 3
    }),
    Some(2), // 并行限制
);

// Runtime 容器部署
let deis_runtime_message = MessageBuilder::runtime_deployment(
    "nginx:1.21".to_string(),
    3, // 副本数
    "production".to_string(), // 命名空间
    "500m".to_string(), // CPU
    "1Gi".to_string(),  // 内存
);

// Proxy 数据管道
let deis_proxy_message = MessageBuilder::proxy_pipeline(
    "analytics_pipeline".to_string(),
    "数据处理管道".to_string(),
    pipeline_stages,
);
```

## API 参考

### 主要类型

- `WireClient`: 与 control 通信的主客户端
- `ControlConfig`: Control 连接配置
- `NodeInfo`: Node 实例信息
- `NodeRole`: Node 类型（Runtime 或 Proxy）
- `Message`: 所有消息类型的 trait
- `WireMessage<M>`: 通用消息包装器
- `MessageBuilder`: 便捷的消息创建

### 主要方法

- `connect()`: 建立 WebSocket 连接
- `authenticate()`: 与 control 认证
- `register()`: 向 control 注册 node
- `send_heartbeat()`: 发送心跳消息
- `run()`: 启动主客户端循环
- `disconnect()`: 断开与 control 的连接

### 配置

`ControlConfig` 支持以下配置选项：

- `associated_id`: Node 实例 ID
- `secret`: 64 字符的十六进制 PSK 密钥
- `addr`: Control WebSocket 地址 (ws:// 或 wss://)

### 消息系统

- 类型安全的消息创建和处理
- 自定义消息类型的可扩展架构
- 内置序列化/反序列化
- 请求-响应关联支持
- 时间戳和 ID 跟踪

## 示例

查看 `examples/` 目录获取更详细的使用示例：

- `extensible_messages.rs`: 演示可扩展消息系统

## 许可证

本项目基于 MIT 许可证 - 查看 [LICENSE](LICENSE) 文件了解详情。 