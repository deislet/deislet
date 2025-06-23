# 统一可扩展消息系统

WorkIt 框架采用基于关联类型的统一消息系统，提供类型安全、可扩展性和高性能的消息处理能力。

## 设计原则

### 核心理念

1. **类型安全**：编译时检查消息类型和数据结构
2. **可扩展性**：新系统可以定义自己的消息类型而无需修改核心库
3. **统一性**：所有消息类型使用相同的基础结构
4. **性能**：零成本抽象，无运行时开销
5. **简洁性**：保持库的简单性，避免复杂的向后兼容层

### 架构概述

所有消息都基于以下核心组件：

- **`Message` Trait**：定义消息类型的公共接口
- **`WorkitMessage<M>`**：通用消息包装器
- **`MessageBuilder`**：统一的消息构建器
- **扩展模块**：特定系统的消息类型定义

## 核心组件

### Message Trait

```rust
pub trait Message: Debug + Clone + Serialize + DeserializeOwned + Send + Sync + 'static {
    /// 消息类型标识符
    const MESSAGE_TYPE: &'static str;
    
    /// 关联类型：消息特定数据
    type Data: Debug + Clone + Serialize + DeserializeOwned + Send + Sync;
    
    /// 获取消息数据
    fn data(&self) -> &Self::Data;
    
    /// 获取可选的消息 ID
    fn message_id(&self) -> Option<Uuid>;
    
    /// 检查此消息是否需要认证
    fn requires_auth(&self) -> bool;
    
    /// 从数据创建新消息
    fn new(data: Self::Data) -> Self;
    
    /// 使用显式 ID 创建新消息
    fn with_id(data: Self::Data, message_id: Uuid) -> Self;
}
```

### WorkitMessage 包装器

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkitMessage<M: Message> {
    /// 消息类型标识符
    pub message_type: String,
    /// 可选的消息 ID，用于请求-响应追踪
    pub message_id: Option<Uuid>,
    /// 消息创建时间戳
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// 消息数据
    pub data: M::Data,
    /// 可选的关联 ID，用于响应匹配请求
    pub correlation_id: Option<Uuid>,
}
```

## 核心消息类型

### 认证消息

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthMessage {
    pub auth_data: AuthData,
    pub message_id: Option<Uuid>,
}

impl Message for AuthMessage {
    const MESSAGE_TYPE: &'static str = "authenticate";
    type Data = AuthData;
    // ... 其他实现
}
```

### 注册消息

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterMessage {
    pub register_data: RegisterRequest,
    pub message_id: Option<Uuid>,
}

impl Message for RegisterMessage {
    const MESSAGE_TYPE: &'static str = "register";
    type Data = RegisterRequest;
    // ... 其他实现
}
```

### 心跳消息

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatMessage {
    pub heartbeat_data: HeartbeatRequest,
    pub message_id: Option<Uuid>,
}

impl Message for HeartbeatMessage {
    const MESSAGE_TYPE: &'static str = "heartbeat";
    type Data = HeartbeatRequest;
    // ... 其他实现
}
```

## 扩展消息类型

### Supervisor 系统

```rust
pub mod supervisor {
    use super::*;
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct BatchOperationData {
        pub operation_type: String,
        pub targets: Vec<String>,
        pub config: serde_json::Value,
        pub parallel_limit: Option<u32>,
    }
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SupervisorMessage {
        pub operation_data: BatchOperationData,
        pub message_id: Option<Uuid>,
    }
    
    impl Message for SupervisorMessage {
        const MESSAGE_TYPE: &'static str = "supervisor_operation";
        type Data = BatchOperationData;
        // ... 实现细节
    }
}
```

### Denix 系统

```rust
pub mod denix {
    use super::*;
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DeploymentData {
        pub image: String,
        pub replicas: u32,
        pub namespace: String,
        pub resources: ResourceRequirements,
    }
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DenixMessage {
        pub deployment_data: DeploymentData,
        pub message_id: Option<Uuid>,
    }
    
    impl Message for DenixMessage {
        const MESSAGE_TYPE: &'static str = "denix_deployment";
        type Data = DeploymentData;
        // ... 实现细节
    }
}
```

### Deflux 系统

```rust
pub mod deflux {
    use super::*;
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PipelineData {
        pub pipeline_id: String,
        pub description: String,
        pub stages: Vec<PipelineStage>,
    }
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DefluxMessage {
        pub pipeline_data: PipelineData,
        pub message_id: Option<Uuid>,
    }
    
    impl Message for DefluxMessage {
        const MESSAGE_TYPE: &'static str = "deflux_pipeline";
        type Data = PipelineData;
        // ... 实现细节
    }
}
```

## 使用方法

### 创建消息

使用 `MessageBuilder` 创建各种类型的消息：

```rust
use workit::models::messages::{MessageBuilder, WorkitMessage};
use workit::models::{AuthData, RegisterRequest, HeartbeatRequest};

// 创建认证消息
let auth_data = AuthData::new("worker_001".to_string(), b"secret_key")?;
let auth_message = MessageBuilder::auth(auth_data);

// 创建注册消息
let register_data = RegisterRequest {
    id: "worker_001".to_string(),
    info: worker_info,
};
let register_message = MessageBuilder::register(register_data);

// 创建心跳消息
let heartbeat_data = HeartbeatRequest {
    id: "worker_001".to_string(),
    power_reserve: 85,
};
let heartbeat_message = MessageBuilder::heartbeat(heartbeat_data);
```

### 创建扩展消息

```rust
use serde_json::json;

// Supervisor 批量操作
let supervisor_message = MessageBuilder::supervisor_operation(
    "batch_deploy".to_string(),
    vec!["worker1".to_string(), "worker2".to_string()],
    json!({
        "image": "myapp:v1.2.3",
        "replicas": 3,
        "strategy": "rolling_update"
    }),
    Some(2), // 并行限制
);

// Denix 容器部署
let denix_message = MessageBuilder::denix_deployment(
    "nginx:1.21".to_string(),
    3,                      // replicas
    "production".to_string(), // namespace
    "500m".to_string(),     // CPU
    "1Gi".to_string(),      // Memory
);

// Deflux 数据管道
let deflux_message = MessageBuilder::deflux_pipeline(
    "analytics_pipeline".to_string(),
    "实时数据分析管道".to_string(),
    vec![
        PipelineStage {
            name: "extract".to_string(),
            stage_type: "data_source".to_string(),
            config: json!({
                "source_type": "database",
                "connection": "postgresql://localhost:5432/data"
            }),
        },
        // ... 更多阶段
    ],
);
```

### 请求-响应模式

```rust
use uuid::Uuid;

// 创建请求
let request_id = Uuid::new_v4();
let auth_data = AuthData::new("worker_001".to_string(), b"secret_key")?;
let auth_msg = AuthMessage::with_id(auth_data, request_id);
let request = WorkitMessage::new(auth_msg);

// 创建响应
let response_data = AuthResponseData {
    authenticated: true,
    message: "认证成功".to_string(),
    error: None,
};
let auth_response = AuthResponseMessage::new(response_data);
let response = WorkitMessage::response_to(auth_response, &request);

// 验证关联
assert_eq!(response.correlation_id, Some(request_id));
```

### 序列化和反序列化

```rust
// 序列化
let json_string = serde_json::to_string(&message)?;

// 反序列化
let deserialized: WorkitMessage<AuthMessage> = serde_json::from_str(&json_string)?;
```

## 自定义消息类型

创建自定义消息类型非常简单：

```rust
// 定义数据结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringData {
    pub metric_name: String,
    pub value: f64,
    pub tags: std::collections::HashMap<String, String>,
}

// 定义消息类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringMessage {
    pub monitoring_data: MonitoringData,
    pub message_id: Option<Uuid>,
}

// 实现 Message trait
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
```

## 最佳实践

### 1. 消息类型命名

- 使用清晰、描述性的名称
- 遵循 `snake_case` 命名约定
- 包含系统前缀（如 `supervisor_`, `denix_`, `deflux_`）

### 2. 数据结构设计

- 保持数据结构简单和可序列化
- 使用适当的类型（如 `String` 而不是 `&str`）
- 为可选字段使用 `Option<T>`

### 3. 错误处理

- 总是处理序列化/反序列化错误
- 提供有意义的错误消息
- 使用 `Result<T, E>` 进行错误传播

### 4. 性能考虑

- 避免不必要的克隆
- 使用引用当可能时
- 考虑使用 `Arc<T>` 共享数据

## 示例代码

完整的示例代码可以在 `examples/extensible_messages.rs` 中找到，展示了：

- 核心消息类型的使用
- 扩展消息类型的创建
- 序列化和反序列化
- 请求-响应模式
- 自定义消息类型的实现

## 迁移指南

如果你之前使用了基于枚举的消息系统，迁移到新的统一系统很简单：

### 旧方式（已移除）
```rust
// 这种方式不再支持
let msg = ClientMessage::authenticate(auth_data);
```

### 新方式
```rust
// 使用统一的消息构建器
let msg = MessageBuilder::auth(auth_data);
```

新系统提供了更好的类型安全性、更清晰的代码结构和更好的可扩展性。 