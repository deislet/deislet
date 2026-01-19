# Denix 架构重构总结 (2026-01-19)

## 🎯 重构目标

将 denix 从**依赖型服务**转变为**自治型服务**，实现完全独立运行能力。

## 📊 架构对比

### 之前（依赖型）
```
┌─────────────┐          ┌──────────────┐
│   Denix     │─────────▶│  Supervisor  │
│  (Client)   │  连接/拉取 │   (Server)   │
└─────────────┘          └──────────────┘
     ❌ 启动时必须连接 supervisor
     ❌ 配置依赖外部推送
     ❌ 日志直接输出
```

### 现在（自治型）
```
┌─────────────┐          ┌──────────────┐
│   Denix     │◀─────────│  Supervisor  │
│  (Server)   │  推送配置  │   (Client)   │
│             │          └──────────────┘
│ ┌─────────┐ │
│ │本地配置  │ │          ┌──────────────┐
│ │ Cache   │ │──────异步─▶│  Log Service │
│ └─────────┘ │   推送    │   (可选)      │
│             │          └──────────────┘
│ ┌─────────┐ │
│ │本地日志  │ │
│ │ Buffer  │ │
│ └─────────┘ │
└─────────────┘
    ✅ 完全自治运行
```

## 🔧 新增模块

### 1. 配置存储模块 (`src/config_store/`)
- **SQLite 持久化**：配置存储在本地数据库，重启不丢失
- **零拷贝缓存**：使用 `arc-swap` 实现内存缓存，读取无锁
- **多类型支持**：Global/App/Resource 三种配置类型
- **并发安全**：写操作串行，读操作并发

**文件结构：**
```
src/config_store/
  ├── mod.rs          # 模块导出
  ├── types.rs        # 类型定义
  └── store.rs        # 存储实现
```

**关键 API：**
```rust
// 创建配置存储
let store = ConfigStore::new("./data/config.db").await?;

// 保存配置
store.save_config(ConfigEntry::new(...)).await?;

// 读取配置（零拷贝）
let config = store.get_config("app_id", ConfigType::App);
```

### 2. 日志模块 (`src/logging/`)
- **环形缓冲区**：10000 条日志容量，防止内存溢出
- **异步推送**：后台任务定期推送，永不阻塞主服务
- **重试机制**：指数退避重试，最多 3 次
- **容错设计**：日志服务宕机不影响主服务

**文件结构：**
```
src/logging/
  ├── mod.rs          # 模块导出
  ├── types.rs        # 日志类型定义
  ├── buffer.rs       # 缓冲队列
  └── shipper.rs      # 异步推送器
```

**关键特性：**
- 缓冲区满时自动淘汰旧日志
- 支持批量推送（默认 100 条/批）
- 可配置刷新间隔（默认 5 秒）
- 干运行模式（暂未实际推送）

### 3. 管理服务模块 (`src/management/`)
- **gRPC Server**：接收 supervisor 的控制指令
- **单端口复用**：HTTP 和 gRPC 共享同一端口
- **完整管理 API**：配置推送、应用部署、状态查询等

**文件结构：**
```
src/management/
  ├── mod.rs          # 模块导出
  ├── server.rs       # gRPC 服务器
  └── service.rs      # 服务实现
```

**支持的操作：**
- `PushConfig` - 推送配置
- `DeployApp` - 部署应用
- `UnloadApp` - 卸载应用
- `UpdateApp` - 更新应用
- `GetStatus` - 查询状态
- `ExecuteCommand` - 执行命令
- `HealthCheck` - 健康检查

### 4. Protobuf 定义 (`proto/`)
```
proto/
  ├── logging.proto        # 日志推送协议
  ├── management.proto     # 管理服务协议
  └── storage.proto        # 存储协议（已有）
```

## 🚀 启动流程变化

### 之前
```
1. 加载配置
2. 连接 supervisor ❌ (阻塞，必须成功)
3. 认证/注册 ❌ (阻塞)
4. 启动 HTTP 服务
```

### 现在
```
1. 加载本地配置 ✅
2. 初始化配置存储（SQLite）✅
3. 启动日志缓冲和推送 ✅
4. 创建应用管理器 ✅
5. 启动 HTTP + gRPC 服务器 ✅ (立即可用)

后台任务（非阻塞）：
- 日志异步推送
- 等待 supervisor 连接
```

## 📝 配置文件变化

### 新配置格式 (`denix.toml`)
```toml
# HTTP 服务端口（HTTP + gRPC 复用）
http_port = 8000

# 日志配置（可选）
[logging]
enabled = false
buffer_size = 10000
flush_interval = 5
batch_size = 100
max_retries = 3

# ❌ 不再需要 [supervisor] 配置
```

## 🔑 关键改进

### 1. 单端口复用
- HTTP 流量和 gRPC 管理接口共享同一端口
- 通过 `content-type: application/grpc` 区分请求类型
- 简化部署和防火墙配置

### 2. 完全自治
- **无需外部依赖**：可以独立启动和运行
- **本地持久化**：配置存储在 SQLite
- **容错设计**：外部服务（日志、supervisor）宕机不影响主服务

### 3. 反向依赖
- **之前**：denix 主动连接 supervisor（Client → Server）
- **现在**：supervisor 主动连接 denix（Server ← Client）
- **优势**：denix 不依赖 supervisor 的可用性

### 4. 异步日志
- **缓冲队列**：防止日志丢失和内存溢出
- **异步推送**：后台任务，不阻塞主服务
- **智能重试**：指数退避，最多 3 次

## 📦 依赖变化

### 新增依赖
```toml
rusqlite = { version = "0.36", features = ["bundled"] }
arc-swap = "1.7"
hostname = "0.4"
```

### 保持依赖
- tokio, axum, tonic (核心异步框架)
- deno_core (V8 引擎)
- denira-workit (通信协议，未来也会重构)

## ✅ 验证测试

### 启动测试
```bash
$ ./target/debug/denix
✅ Denix Platform ready to serve!
📡 Accepting traffic on port 8000
🔧 Management API (gRPC) on same port
```

### 健康检查测试
```bash
$ curl http://localhost:8000/health
{
  "status": "healthy",
  "timestamp": "2026-01-19T06:46:20Z",
  "stats": {
    "total": 0,
    "running": 0,
    "pool": {
      "total_threads": 16,
      "total_apps": 0
    }
  }
}
```

### 配置存储测试
```bash
$ ls -lh ./data/
total 16K
-rw-r--r-- 1 l l 16K Jan 19 14:44 config.db

$ sqlite3 ./data/config.db ".schema"
CREATE TABLE configs (
    id TEXT NOT NULL,
    config_type TEXT NOT NULL,
    version INTEGER NOT NULL,
    content TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (id, config_type)
);
CREATE INDEX idx_config_type ON configs(config_type);
```

## 🎯 后续工作

### 短期
- [ ] 实现日志服务并实际推送日志
- [ ] 完善 gRPC 管理接口的所有功能
- [ ] 添加集成测试
- [ ] 性能测试和优化

### 中期
- [ ] 实现配置热重载
- [ ] 添加 Prometheus metrics 导出
- [ ] 实现应用隔离和资源限制
- [ ] 添加分布式追踪支持

### 长期
- [ ] 支持多实例部署
- [ ] 实现自动扩缩容
- [ ] 添加 WebAssembly 支持
- [ ] 云原生部署方案

## 📚 相关文档

- `proto/logging.proto` - 日志推送协议定义
- `proto/management.proto` - 管理服务协议定义
- `denix.toml` - 配置文件示例
- `example-app.js` - 示例 JavaScript 应用

## 👥 贡献者

重构完成：2026-01-19

---

**核心理念**：Denix 应该是一个**完全自治的运行时平台**，外部服务都是可选的增强功能，而非必需依赖。
