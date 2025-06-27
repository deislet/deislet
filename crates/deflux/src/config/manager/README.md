# 配置管理器技术文档

## 概述

Deflux 配置管理器是一个高性能、分阶段的配置管理系统，支持热更新和无锁读取。

## 架构设计

### 分阶段启动流程

```
Pending → 本地配置加载 → Supervisor注册 → 配置同步 → 初始化 → Active
```

1. **Pending 状态**: 系统启动，等待配置加载
2. **本地配置加载**: 读取本地 TOML 配置文件
3. **Supervisor 注册**: 连接管理服务器并注册实例
4. **配置同步**: 拉取服务器下发的系统和应用配置
5. **配置叠加**: 合并默认、本地、服务器配置
6. **初始化**: 加载应用配置到内存缓存
7. **Active 状态**: 系统准备就绪，开始处理请求

### 配置分层

```
最终配置 = 默认配置 + 本地配置 + Supervisor配置
```

- **默认配置**: 硬编码的默认值
- **本地配置**: 本地 TOML 文件配置
- **Supervisor配置**: 服务器下发的配置

### 存储架构

#### 系统配置
- **存储位置**: 内存中，使用 `Arc<RwLock<SystemConfig>>`
- **更新方式**: 分层叠加，异步写锁更新
- **访问方式**: 读锁保护的同步访问

#### 应用配置
- **持久化**: SQLite 数据库表 `app_configs`
- **内存缓存**: `ArcSwap<HashMap<String, JsonValue>>`
- **更新方式**: 事务中同步更新数据库和缓存
- **访问方式**: 无锁原子读取

## 数据库设计

### 表结构

```sql
CREATE TABLE app_configs (
    app_id TEXT PRIMARY KEY,
    config TEXT NOT NULL,           -- JSON 格式配置
    version_timestamp TEXT NOT NULL -- RFC3339 时间戳
);
```

### 增量更新机制

1. **版本追踪**: 每个配置变更都有唯一的 RFC3339 时间戳
2. **增量查询**: `SELECT * FROM app_configs WHERE version_timestamp > ?`
3. **批量更新**: 在单个事务中应用多个配置变更
4. **原子同步**: 数据库更新和 ArcSwap 更新在同一事务中完成

## API 设计

### 核心接口

```rust
impl ConfigManager {
    /// 创建配置管理器
    pub async fn new(
        config_path: PathBuf, 
        database: ConfigDatabase
    ) -> Result<Self, ConfigError>;
    
    /// 获取系统配置
    pub async fn get_system_config(&self) -> SystemConfig;
    
    /// 更新应用配置（事务）
    pub async fn update_app_configs(
        &self, 
        updates: Vec<AppConfigEntry>
    ) -> Result<DateTime<Utc>, ConfigError>;
    
    /// 获取应用配置缓存引用
    pub fn get_app_configs_arc(&self) -> Arc<ArcSwap<HashMap<String, JsonValue>>>;
    
    /// 获取单个应用配置
    pub fn get_app_config(&self, app_id: &str) -> Option<JsonValue>;
}
```

### 数据库接口

```rust
impl ConfigDatabase {
    /// 初始化数据库
    pub async fn init<P: AsRef<Path>>(path: P) -> Result<Self, DatabaseError>;
    
    /// 加载所有应用配置
    pub async fn load_all_app_configs(&self) -> Result<HashMap<String, JsonValue>, DatabaseError>;
    
    /// 获取最新版本时间戳
    pub async fn get_latest_version_timestamp(&self) -> Result<Option<DateTime<Utc>>, DatabaseError>;
    
    /// 获取增量更新
    pub async fn get_app_configs_after(
        &self, 
        timestamp: DateTime<Utc>
    ) -> Result<Vec<AppConfigEntry>, DatabaseError>;
    
    /// 应用增量更新（事务）
    pub async fn apply_incremental_updates<F>(
        &mut self, 
        updates: Vec<AppConfigEntry>, 
        callback: F
    ) -> Result<DateTime<Utc>, DatabaseError>
    where F: FnOnce(&HashMap<String, JsonValue>) -> Result<(), DatabaseError>;
}
```

## 性能特性

### 零锁开销读取

```rust
// 获取配置引用（零成本）
let app_configs_arc = config_manager.get_app_configs_arc();

// 原子读取（无锁）
let configs = app_configs_arc.load();

// 直接访问 HashMap
if let Some(config) = configs.get("app1") {
    // 使用配置...
}
```

### 原子更新

```rust
// 准备更新
let updates = vec![
    AppConfigEntry {
        app_id: "app1".to_string(),
        config: json!({"enabled": true}),
        version_timestamp: Utc::now(),
    }
];

// 事务更新（原子）
let latest_version = config_manager.update_app_configs(updates).await?;
```

### 内存效率

- **共享读取**: 多个线程共享同一个 HashMap，零拷贝
- **写时复制**: 更新时创建新的 HashMap，旧版本自动回收
- **引用计数**: 使用 Arc 自动管理内存生命周期

## 并发安全

### 读写分离

- **读操作**: ArcSwap 提供无锁、无阻塞的读取
- **写操作**: 数据库事务 + 原子指针交换
- **一致性**: 读取总是看到完整、一致的配置快照

### 事务保证

```rust
// 在事务中同步更新
db.apply_incremental_updates(updates, |new_configs| {
    // 更新内存缓存
    arc_swap.store(Arc::new(new_configs.clone()));
    Ok(())
}).await?;
```

## 测试覆盖

### 单元测试

- **配置管理器创建**: ✅
- **系统配置读取**: ✅  
- **应用配置更新**: ✅
- **增量更新机制**: ✅
- **ArcSwap 并发访问**: ✅
- **数据库事务**: ✅

### 集成测试

- **代理服务集成**: ✅
- **配置热更新**: ✅
- **错误处理**: ✅
- **性能测试**: ✅

## 最佳实践

### 配置读取

```rust
// ✅ 推荐: 使用 ArcSwap 无锁读取
let configs = config_manager.get_app_configs_arc().load();
let app_config = configs.get("app1");

// ❌ 避免: 频繁调用 get_app_config
let app_config = config_manager.get_app_config("app1");
```

### 批量更新

```rust
// ✅ 推荐: 批量更新多个配置
let updates = vec![update1, update2, update3];
config_manager.update_app_configs(updates).await?;

// ❌ 避免: 逐个更新
for update in updates {
    config_manager.update_app_configs(vec![update]).await?;
}
```

### 错误处理

```rust
// ✅ 推荐: 正确处理错误
match config_manager.update_app_configs(updates).await {
    Ok(timestamp) => println!("更新成功: {}", timestamp),
    Err(ConfigError::DatabaseError(e)) => {
        eprintln!("数据库错误: {}", e);
        // 重试或降级处理
    }
    Err(e) => eprintln!("其他错误: {}", e),
}
```

## 未来优化

### 性能优化

- **批量读取**: 支持批量获取多个应用配置
- **缓存预热**: 启动时预加载热点配置
- **压缩存储**: 对大型配置进行压缩存储

### 功能扩展

- **配置版本控制**: 支持配置回滚和历史查询
- **配置验证**: 运行时配置语法和语义验证
- **配置模板**: 支持配置模板和变量替换

---

*文档版本: v1.0*  
*最后更新: 2024-12-19* 