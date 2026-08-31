# Proxy WAF 实现方案建议

**文档版本**: 1.0
**创建日期**: 2026-01-16
**目标组件**: Proxy (Load Balancer)

---

## 执行摘要

基于对 `pingora-waf`、`pingap`、`aralez` 和官方 Pingora 框架的深入分析，本文档提出了为 Proxy 添加 WAF（Web Application Firewall）功能的三个实现方案，并给出推荐方案和实施路线图。

**核心需求**:
- SQL 注入和 XSS 攻击防护
- 路径遍历和命令注入检测
- 限流和 IP 黑白名单
- 高性能（<10ms 延迟增加）
- 配置热重载（通过 Control）
- Prometheus 指标集成

---

## 1. 项目分析对比

### 1.1 pingora-waf 分析

**架构特点**:
```
✓ 专注 WAF 功能，规则引擎完整
✓ 15K+ req/sec，<7ms 延迟
✓ 基于 Pingora 0.6.0（我们使用 0.5）
✓ 使用 DashMap + Arc 并发设计
✓ YAML 配置文件
✓ 热重载支持（notify crate）
```

**核心实现**:
- **SecurityRule Trait**: 统一的规则接口
- **检测器**: SQL注入、XSS、路径遍历、命令注入、Bot、IP过滤、限流
- **集成点**: `request_filter()`、`request_body_filter()`、`upstream_peer()`
- **性能优化**:
  - 编译时正则表达式（`once_cell::sync::Lazy`）
  - 白名单头部跳过
  - 快速路径优化（短输入快速返回）
  - 分阶段检查（避免重复）

**优势**:
- ✅ 开箱即用的完整 WAF 规则
- ✅ 性能已验证（15K req/sec）
- ✅ 代码质量高，测试覆盖完整
- ✅ 独立模块，易于集成

**劣势**:
- ❌ Pingora 版本不匹配（0.6 vs 0.5）
- ❌ 配置系统与 Proxy 不兼容（YAML vs TOML）
- ❌ 缺乏插件化设计（硬编码规则）
- ❌ 不支持自定义规则扩展

---

### 1.2 pingap 分析

**架构特点**:
```
✓ 完整的插件系统架构
✓ 18+ 种插件（认证、安全、限流、缓存等）
✓ 基于 Pingora git-main（最新）
✓ TOML 配置（与 Proxy 一致）
✓ ArcSwap 配置热更新（与 Proxy 一致）
✓ DashMap 插件注册表
```

**插件系统**:
```rust
#[async_trait]
pub trait Plugin: Sync + Send {
    async fn handle_request(
        &self,
        step: PluginStep,
        session: &mut Session,
        ctx: &mut Ctx,
    ) -> RequestPluginResult;

    async fn handle_response(...) -> ResponsePluginResult;
    fn handle_response_body(...) -> ResponseBodyPluginResult;
}
```

**可用插件**:
- **认证**: JWT、Key Auth、Basic Auth、Combined Auth
- **安全**: CSRF、IP 限制、Referer/UA 限制
- **限流**: Inflight（并发）、Rate（速率）
- **其他**: 缓存、压缩、重定向、请求ID

**优势**:
- ✅ 插件架构高度可扩展
- ✅ 配置系统与 Proxy 兼容
- ✅ 多种现成的安全插件
- ✅ 良好的性能设计（DashMap、ArcSwap）
- ✅ 支持自定义插件

**劣势**:
- ❌ 缺少完整的 WAF 规则（无 SQL/XSS 检测）
- ❌ 需要移植核心检测逻辑
- ❌ 代码复杂度高（3个 crate）
- ❌ 学习成本高

---

### 1.3 aralez 分析

**架构特点**:
```
✓ 轻量级，代码简洁
✓ 专注限流功能
✓ 基于 Pingora 0.6.0
✓ 简单的 TOML 配置
```

**核心功能**:
- 按虚拟主机限流
- 全局限流
- 健康检查

**评估**:
- ⚠️ 功能太单一，不适合作为 WAF 基础
- ⚠️ 可作为限流实现参考
- ⚠️ 代码可读性好，适合学习

---

## 2. 推荐方案

基于 Proxy 的架构特点和需求，**推荐采用混合方案**：

### 方案 A：插件化 WAF 架构（推荐 ⭐）

**核心思路**:
1. 采用 **pingap 的插件系统架构**（高扩展性）
2. 移植 **pingora-waf 的检测规则**（完整的 WAF 能力）
3. 使用 **Proxy 现有的配置系统**（TOML + ArcSwap）
4. 通过 **Control 管理 WAF 配置**（热更新）

**架构设计**:

```
┌─────────────────────────────────────────────────────────────┐
│                        Proxy LB                             │
│  ┌─────────────┐  ┌──────────────┐  ┌─────────────────┐    │
│  │   Pingora   │  │  Config      │  │  Control     │    │
│  │   Proxy     │◄─┤  Manager     │◄─┤  Client         │    │
│  │             │  │  (ArcSwap)   │  │  (gRPC)         │    │
│  └──────┬──────┘  └──────────────┘  └─────────────────┘    │
│         │                                                    │
│         │ ProxyHttp hooks                                   │
│  ┌──────▼───────────────────────────────────────────────┐   │
│  │            WAF Plugin Engine                         │   │
│  │  ┌──────────────────────────────────────────────┐   │   │
│  │  │  Plugin Chain (PluginStep 执行)              │   │   │
│  │  │  1. EarlyRequest  → IP/Bot 过滤             │   │   │
│  │  │  2. Request       → 认证/限流               │   │   │
│  │  │  3. ProxyUpstream → SQL/XSS/路径/命令检测   │   │   │
│  │  └──────────────────────────────────────────────┘   │   │
│  │                                                       │   │
│  │  ┌─────────────┐  ┌─────────────┐  ┌───────────┐   │   │
│  │  │ WAF 插件    │  │ Auth 插件   │  │ 限流插件  │   │   │
│  │  │ - SQL注入   │  │ - JWT       │  │ - Rate    │   │   │
│  │  │ - XSS       │  │ - Key Auth  │  │ - Inflight│   │   │
│  │  │ - 路径遍历  │  │ - IP 白名单 │  │           │   │   │
│  │  │ - 命令注入  │  │             │  │           │   │   │
│  │  └─────────────┘  └─────────────┘  └───────────┘   │   │
│  │                                                       │   │
│  │  ┌──────────────────────────────────────────────┐   │   │
│  │  │ SecurityRule Trait (from pingora-waf)        │   │   │
│  │  │ - check(request, body) -> Result<(), Violation> │ │
│  │  └──────────────────────────────────────────────┘   │   │
│  └───────────────────────────────────────────────────────┘  │
│                                                              │
│  ┌────────────────────────────────────────────────────┐     │
│  │  Metrics Collector (Prometheus)                    │     │
│  │  - waf_total_requests                              │     │
│  │  - waf_blocked_requests{reason}                    │     │
│  │  - waf_plugin_latency{plugin}                      │     │
│  └────────────────────────────────────────────────────┘     │
└─────────────────────────────────────────────────────────────┘
```

**技术栈**:
- **插件系统**: 参考 pingap 的 Plugin Trait + PluginFactory
- **检测规则**: 移植 pingora-waf 的 SecurityRule
- **配置管理**: 扩展 Proxy 的 TOML 配置
- **热更新**: 利用 Proxy 的 ArcSwap + Control 通道
- **并发**: DashMap（插件注册）+ Arc（共享检测器）

---

### 方案 B：直接集成 pingora-waf（快速）

**适用场景**: 快速原型、MVP 验证

**实施步骤**:
1. Fork pingora-waf 并降级到 Pingora 0.5
2. 适配配置系统（YAML → TOML）
3. 集成到 Proxy 的 ProxyHttp 实现
4. 通过 Control 管理配置

**优势**:
- ✅ 实施快速（2-3 周）
- ✅ 规则完整
- ✅ 性能已验证

**劣势**:
- ❌ 不可扩展（硬编码规则）
- ❌ 配置系统不统一
- ❌ 维护成本高（fork 维护）

---

### 方案 C：完全移植 pingap（长期）

**适用场景**: 需要完整的 API 网关能力

**实施步骤**:
1. 移植 pingap 的完整插件系统
2. 移植所有现有插件
3. 添加 pingora-waf 的检测规则作为插件
4. 深度集成配置系统

**优势**:
- ✅ 功能最完整（API 网关级）
- ✅ 高度可扩展
- ✅ 长期维护性好

**劣势**:
- ❌ 实施时间长（2-3 月）
- ❌ 代码复杂度高
- ❌ 可能过度设计

---

## 3. 推荐方案详细设计（方案 A）

### 3.1 模块结构

```
crates/deis-proxy/
├── src/
│   ├── waf/                    # WAF 模块（新增）
│   │   ├── mod.rs              # 模块入口
│   │   ├── plugin.rs           # Plugin Trait 定义
│   │   ├── factory.rs          # PluginFactory
│   │   ├── engine.rs           # WAF 引擎（插件链执行）
│   │   ├── rules/              # 检测规则（移植自 pingora-waf）
│   │   │   ├── mod.rs
│   │   │   ├── sql_injection.rs
│   │   │   ├── xss.rs
│   │   │   ├── path_traversal.rs
│   │   │   ├── command_injection.rs
│   │   │   └── trait.rs        # SecurityRule Trait
│   │   ├── plugins/            # WAF 插件实现
│   │   │   ├── mod.rs
│   │   │   ├── sql_waf.rs      # SQL 注入插件
│   │   │   ├── xss_waf.rs      # XSS 插件
│   │   │   ├── path_waf.rs     # 路径遍历插件
│   │   │   ├── cmd_waf.rs      # 命令注入插件
│   │   │   ├── ip_filter.rs    # IP 过滤插件
│   │   │   ├── bot_filter.rs   # Bot 检测插件
│   │   │   ├── rate_limit.rs   # 限流插件
│   │   │   └── auth.rs         # 认证插件（可选）
│   │   ├── metrics.rs          # Prometheus 指标
│   │   └── config.rs           # WAF 配置结构
│   │
│   ├── proxy/
│   │   ├── mod.rs              # 修改：集成 WAF 引擎
│   │   └── service.rs          # 修改：ProxyHttp hooks
│   │
│   └── config/
│       └── mod.rs              # 修改：添加 WAF 配置项
```

### 3.2 核心接口设计

#### Plugin Trait

```rust
// src/waf/plugin.rs
use async_trait::async_trait;
use pingora::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStep {
    EarlyRequest,    // IP/Bot 过滤
    Request,         // 认证/限流
    ProxyUpstream,   // WAF 检测
}

#[derive(Debug)]
pub enum PluginResult {
    Continue,                   // 继续执行链
    Respond(HttpResponse),      // 返回响应并终止
    Skip,                       // 跳过此插件
}

#[async_trait]
pub trait Plugin: Send + Sync {
    /// 插件名称
    fn name(&self) -> &str;

    /// 执行步骤
    fn step(&self) -> PluginStep;

    /// 请求处理
    async fn handle_request(
        &self,
        session: &mut Session,
        ctx: &mut ProxyContext,
    ) -> Result<PluginResult>;

    /// 请求体处理（可选）
    async fn handle_request_body(
        &self,
        _session: &mut Session,
        _ctx: &mut ProxyContext,
        _body: &[u8],
        _end_of_stream: bool,
    ) -> Result<PluginResult> {
        Ok(PluginResult::Continue)
    }
}

/// 插件执行上下文
pub struct ProxyContext {
    pub violations: Vec<SecurityViolation>,
    pub blocked: bool,
    pub body_buffer: Vec<u8>,
    pub metadata: HashMap<String, String>,
}
```

#### SecurityRule Trait（移植自 pingora-waf）

```rust
// src/waf/rules/trait.rs
pub trait SecurityRule: Send + Sync {
    /// 规则名称
    fn name(&self) -> &str;

    /// 检查请求
    fn check(
        &self,
        request: &RequestHeader,
        body: Option<&[u8]>,
    ) -> Result<(), SecurityViolation>;
}

#[derive(Debug, Clone)]
pub struct SecurityViolation {
    pub rule_name: String,
    pub threat_type: String,
    pub threat_level: ThreatLevel,
    pub description: String,
    pub blocked: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum ThreatLevel {
    Low,
    Medium,
    High,
    Critical,
}
```

#### PluginFactory

```rust
// src/waf/factory.rs
use dashmap::DashMap;
use std::sync::Arc;

pub type PluginCreator = Arc<dyn Fn(&PluginConfig) -> Result<Arc<dyn Plugin>> + Send + Sync>;

pub struct PluginFactory {
    creators: DashMap<String, PluginCreator>,
}

impl PluginFactory {
    pub fn new() -> Self {
        Self {
            creators: DashMap::new(),
        }
    }

    /// 注册插件创建器
    pub fn register<F>(&self, category: &str, creator: F)
    where
        F: Fn(&PluginConfig) -> Result<Arc<dyn Plugin>> + Send + Sync + 'static,
    {
        self.creators.insert(category.to_string(), Arc::new(creator));
    }

    /// 创建插件实例
    pub fn create(&self, config: &PluginConfig) -> Result<Arc<dyn Plugin>> {
        let creator = self.creators
            .get(&config.category)
            .ok_or_else(|| anyhow!("Unknown plugin category: {}", config.category))?;

        creator(config)
    }
}

// 全局单例
static PLUGIN_FACTORY: Lazy<PluginFactory> = Lazy::new(|| {
    let factory = PluginFactory::new();

    // 自动注册所有插件
    register_all_plugins(&factory);

    factory
});
```

#### WAF Engine

```rust
// src/waf/engine.rs
pub struct WafEngine {
    plugins: Vec<Arc<dyn Plugin>>,
    config: Arc<WafConfig>,
    metrics: Arc<WafMetrics>,
}

impl WafEngine {
    /// 按步骤执行插件链
    pub async fn execute_step(
        &self,
        step: PluginStep,
        session: &mut Session,
        ctx: &mut ProxyContext,
    ) -> Result<bool> {
        for plugin in self.plugins.iter().filter(|p| p.step() == step) {
            let start = Instant::now();

            match plugin.handle_request(session, ctx).await? {
                PluginResult::Continue => {
                    // 记录插件执行时间
                    let elapsed = start.elapsed().as_millis() as u64;
                    self.metrics.record_plugin_latency(plugin.name(), elapsed);
                    continue;
                }
                PluginResult::Respond(response) => {
                    // 插件返回响应，终止链
                    self.metrics.increment_blocked(plugin.name());
                    response.send(session).await?;
                    return Ok(true);  // 已处理
                }
                PluginResult::Skip => continue,
            }
        }

        Ok(false)  // 未处理，继续正常流程
    }
}
```

### 3.3 Pingora ProxyHttp 集成

```rust
// src/proxy/service.rs
#[async_trait]
impl ProxyHttp for DeisProxy {
    type CTX = ProxyContext;

    fn new_ctx(&self) -> Self::CTX {
        ProxyContext {
            violations: Vec::new(),
            blocked: false,
            body_buffer: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    // 早期请求阶段：IP/Bot 过滤
    async fn early_request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(waf) = &self.waf_engine {
            if waf.execute_step(PluginStep::EarlyRequest, session, ctx).await? {
                return Ok(());  // 已被 WAF 处理
            }
        }
        Ok(())
    }

    // 请求阶段：认证/限流
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<bool> {
        if let Some(waf) = &self.waf_engine {
            if waf.execute_step(PluginStep::Request, session, ctx).await? {
                return Ok(true);  // 已被 WAF 处理
            }
        }
        Ok(false)  // 继续正常流程
    }

    // 请求体过滤：累积 body
    async fn request_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(chunk) = body {
            ctx.body_buffer.extend_from_slice(chunk);
        }

        // 只在接收完整 body 后检查
        if end_of_stream {
            if let Some(waf) = &self.waf_engine {
                for plugin in waf.plugins.iter() {
                    plugin.handle_request_body(
                        session,
                        ctx,
                        &ctx.body_buffer,
                        true,
                    ).await?;
                }
            }
        }

        Ok(())
    }

    // 代理到上游前：最后的 WAF 检查
    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // WAF 检测
        if let Some(waf) = &self.waf_engine {
            if waf.execute_step(PluginStep::ProxyUpstream, session, ctx).await? {
                // 如果被阻止，返回错误上游（不会实际连接）
                return Err(Error::new(ErrorType::HTTPStatus(403)));
            }
        }

        // 正常负载均衡逻辑
        self.select_upstream(session, ctx)
    }
}
```

### 3.4 配置系统

#### TOML 配置格式

```toml
# deis-proxy.toml

# WAF 全局配置
[waf]
enabled = true
max_body_size = 1048576  # 1MB

# 插件配置
[waf.plugins]

# SQL 注入检测
[waf.plugins.sql_injection]
enabled = true
block_mode = true  # true=阻止, false=仅记录
check_uri = true
check_headers = true
check_body = true

# XSS 检测
[waf.plugins.xss]
enabled = true
block_mode = true

# 路径遍历检测
[waf.plugins.path_traversal]
enabled = true
block_mode = true

# IP 过滤
[waf.plugins.ip_filter]
enabled = true
whitelist = ["10.0.0.0/8", "192.168.0.0/16"]
blacklist = ["192.168.1.100"]

# Bot 检测
[waf.plugins.bot_detection]
enabled = true
block_bad_bots = true
allow_known_bots = true
custom_bad_bots = ["badbot", "scanner"]

# 限流（按 IP）
[waf.plugins.rate_limit]
enabled = true
type = "ip"
max_requests = 100
window_secs = 60

# JWT 认证（可选）
[waf.plugins.jwt_auth]
enabled = false
secret = "your-secret-key"
algorithm = "HS256"
header = "Authorization"
```

#### Rust 配置结构

```rust
// src/waf/config.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WafConfig {
    pub enabled: bool,
    pub max_body_size: usize,
    pub plugins: HashMap<String, PluginConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginConfig {
    pub enabled: bool,
    pub category: String,
    pub priority: Option<i32>,  // 执行优先级
    #[serde(flatten)]
    pub settings: HashMap<String, serde_json::Value>,
}

// 集成到 Proxy 的主配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyConfig {
    // 现有字段...
    pub waf: Option<WafConfig>,  // 新增
}
```

### 3.5 Control 集成

```rust
// src/control/client.rs

// 新增 Control 命令
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlCommand {
    // 现有命令...
    UpdateWafConfig(WafConfig),
    ReloadWafRules,
    EnableWafPlugin { plugin: String },
    DisableWafPlugin { plugin: String },
}

impl ProxyControlClient {
    // 处理 WAF 配置更新
    async fn handle_waf_update(&mut self, config: WafConfig) -> Result<()> {
        info!("Applying WAF configuration update");

        // 使用 ArcSwap 原子更新
        let new_waf_engine = WafEngine::from_config(&config)?;
        self.waf_engine.store(Arc::new(new_waf_engine));

        info!("WAF configuration updated successfully");
        Ok(())
    }
}
```

### 3.6 指标集成

```rust
// src/waf/metrics.rs
use prometheus::{IntCounter, IntCounterVec, HistogramVec, Registry};

pub struct WafMetrics {
    pub total_requests: IntCounter,
    pub blocked_requests: IntCounterVec,      // 按 reason 标签
    pub plugin_latency: HistogramVec,         // 按 plugin 标签
    pub violations_detected: IntCounterVec,   // 按 violation_type 标签
}

impl WafMetrics {
    pub fn new(registry: &Registry) -> Result<Self> {
        let total = IntCounter::new(
            "deis_proxy_waf_total_requests",
            "Total requests processed by WAF"
        )?;

        let blocked = IntCounterVec::new(
            Opts::new("deis_proxy_waf_blocked_requests", "Blocked requests by reason"),
            &["reason"]
        )?;

        let latency = HistogramVec::new(
            HistogramOpts::new("deis_proxy_waf_plugin_latency_ms", "Plugin execution latency"),
            &["plugin"]
        )?;

        registry.register(Box::new(total.clone()))?;
        registry.register(Box::new(blocked.clone()))?;
        registry.register(Box::new(latency.clone()))?;

        Ok(Self {
            total_requests: total,
            blocked_requests: blocked,
            plugin_latency: latency,
            violations_detected: violations,
        })
    }
}
```

---

## 4. 实施计划

### Phase 1: 基础架构（2-3 周）

**目标**: 搭建插件系统骨架

- [ ] 定义 Plugin Trait 和 PluginFactory
- [ ] 实现 WAF Engine（插件链执行）
- [ ] 集成到 ProxyHttp hooks
- [ ] 配置系统扩展（TOML 支持）
- [ ] 基础测试框架

**交付物**:
- 可运行的插件框架
- 1-2 个示例插件（如 IP 过滤）
- 单元测试覆盖

### Phase 2: 核心 WAF 规则（3-4 周）

**目标**: 移植 pingora-waf 的检测规则

- [ ] 移植 SecurityRule Trait
- [ ] 实现 SQL 注入检测（15+ 正则）
- [ ] 实现 XSS 检测（13+ 正则）
- [ ] 实现路径遍历检测（15+ 正则）
- [ ] 实现命令注入检测（30+ 正则）
- [ ] 将规则包装为插件

**交付物**:
- 4 个核心 WAF 插件
- 完整的规则测试套件
- 性能基准测试

### Phase 3: 安全增强（2 周）

**目标**: 添加辅助安全功能

- [ ] IP 过滤插件（白名单/黑名单/CIDR）
- [ ] Bot 检测插件（30+ Bot 模式）
- [ ] 限流插件（Rate + Inflight）
- [ ] 请求体大小限制
- [ ] 时序攻击防御

**交付物**:
- 3 个安全插件
- 集成测试

### Phase 4: 可观测性（1 周）

**目标**: 完善监控和日志

- [ ] Prometheus 指标集成
- [ ] 结构化日志（违规详情）
- [ ] Grafana 仪表盘模板
- [ ] 性能分析工具

**交付物**:
- 完整的指标收集
- Grafana dashboard JSON
- 日志查询示例

### Phase 5: Control 集成（1-2 周）

**目标**: 支持动态配置管理

- [ ] Control 协议扩展（WAF 命令）
- [ ] 配置热更新（ArcSwap）
- [ ] Control Web UI 集成（WAF 管理页面）
- [ ] 规则验证和测试

**交付物**:
- Control 命令支持
- Web UI WAF 配置页面
- 配置验证 API

### Phase 6: 生产就绪（2 周）

**目标**: 性能优化和文档

- [ ] 性能优化（目标 <10ms 延迟）
- [ ] 压力测试（15K+ req/sec）
- [ ] 文档编写（配置指南、规则说明）
- [ ] 示例配置
- [ ] 故障排查指南

**交付物**:
- 性能报告
- 完整文档
- 生产配置模板

---

## 5. 性能目标

### 5.1 延迟目标

| 场景 | 目标延迟 | 测量方式 |
|------|---------|----------|
| 无 WAF | 基线 | wrk 基准测试 |
| 仅 IP 过滤 | +0.5ms | 单插件开销 |
| 完整 WAF（无 body）| +3-5ms | Header 检查 |
| 完整 WAF（有 body）| +8-10ms | Header + Body 检查 |

### 5.2 吞吐量目标

- **最低目标**: 10K req/sec（与 pingora-waf 持平）
- **理想目标**: 15K+ req/sec（匹配 pingora-waf）
- **负载条件**: 4 核 CPU, 8GB RAM

### 5.3 资源消耗

- **内存**: <200MB（含所有规则和缓存）
- **CPU**: <50%（在 10K req/sec 负载下）
- **缓存**: DashMap 限流缓存 <50MB

---

## 6. 风险和缓解

### 6.1 Pingora 版本兼容性

**风险**: Proxy 使用 Pingora 0.5，而 pingora-waf 基于 0.6

**缓解**:
- 仅移植检测逻辑（正则表达式和检测函数）
- 插件接口基于稳定的 ProxyHttp trait
- 隔离版本依赖的代码

### 6.2 性能影响

**风险**: WAF 检测可能增加显著延迟

**缓解**:
- 分阶段检查（EarlyRequest → Request → ProxyUpstream）
- 白名单优化（跳过已知安全的路径/IP）
- 编译时正则表达式
- 可配置的插件启用/禁用

### 6.3 误报率

**风险**: WAF 规则可能阻止合法请求

**缓解**:
- 两种模式：block（阻止）和 log（仅记录）
- 详细的违规日志
- 白名单机制
- 规则可配置

### 6.4 维护成本

**风险**: 需要持续更新 WAF 规则

**缓解**:
- 模块化设计（易于更新单个规则）
- 完善的测试覆盖
- 参考 OWASP Core Rule Set
- 社区贡献机制

---

## 7. 替代方案

### 7.1 使用外部 WAF（如 ModSecurity）

**优势**:
- 成熟的规则集
- 社区支持

**劣势**:
- 性能较差（3x 慢于 pingora-waf）
- 集成复杂
- 不支持 Rust 生态

### 7.2 完全自研 WAF

**优势**:
- 完全控制
- 深度优化

**劣势**:
- 开发时间长（6+ 月）
- 规则不成熟
- 测试覆盖不足

### 7.3 使用云 WAF（如 Cloudflare）

**优势**:
- 零维护
- 专业团队支持

**劣势**:
- 成本高
- 数据隐私问题
- 不适合私有部署

---

## 8. 成功标准

### 8.1 功能完整性

- ✅ 支持 SQL 注入检测（100+ 模式）
- ✅ 支持 XSS 检测（50+ 模式）
- ✅ 支持路径遍历检测
- ✅ 支持命令注入检测
- ✅ IP 黑白名单
- ✅ Bot 检测
- ✅ 限流（Rate + Inflight）

### 8.2 性能指标

- ✅ 延迟增加 <10ms（P99）
- ✅ 吞吐量 >10K req/sec
- ✅ 内存占用 <200MB
- ✅ CPU 使用 <50%（10K req/sec 负载）

### 8.3 可用性

- ✅ 配置热更新（通过 Control）
- ✅ 两种模式（block/log）
- ✅ Web UI 管理界面
- ✅ Prometheus 指标
- ✅ 完整文档

### 8.4 测试覆盖

- ✅ 单元测试覆盖率 >80%
- ✅ 集成测试覆盖所有插件
- ✅ 性能基准测试
- ✅ 压力测试（wrk/k6）

---

## 9. 下一步行动

### 立即行动（本周）

1. **技术决策确认**
   - [ ] 评审本方案
   - [ ] 确认技术选型
   - [ ] 确定优先级

2. **环境准备**
   - [ ] 创建 `crates/deis-proxy/src/waf/` 目录结构
   - [ ] 添加依赖到 Cargo.toml（regex, once_cell, dashmap 等）
   - [ ] 设置基准测试环境

### 短期目标（2 周内）

1. **Phase 1 启动**
   - [ ] 实现 Plugin Trait
   - [ ] 实现 PluginFactory
   - [ ] 第一个示例插件（IP 过滤）

2. **原型验证**
   - [ ] 集成到 ProxyHttp
   - [ ] 基础性能测试
   - [ ] 内部评审

---

## 10. 参考资源

### 代码参考

- **pingora-waf**: `/mnt/sdb1/deislet/deislet/examples/pingora-waf/`
  - `src/waf/sql_injection.rs` - SQL 注入规则
  - `src/waf/xss_detector.rs` - XSS 规则
  - `src/proxy/mod.rs` - ProxyHttp 集成

- **pingap**: `/mnt/sdb1/deislet/deislet/examples/pingap/`
  - `pingap-core/src/plugin.rs` - Plugin Trait
  - `pingap-plugin/src/plugin.rs` - PluginFactory
  - `pingap-proxy/src/server.rs` - 插件链执行

### 文档

- [Pingora 官方文档](https://github.com/cloudflare/pingora/tree/main/docs)
- [OWASP ModSecurity Core Rule Set](https://owasp.org/www-project-modsecurity-core-rule-set/)
- [OWASP Top 10](https://owasp.org/www-project-top-ten/)

### 工具

- **测试**: wrk, k6, siege
- **监控**: Prometheus, Grafana
- **规则测试**: curl, burp suite

---

## 附录 A：关键依赖

这份附录是 2026-01-16 写的，当时 deis-proxy 用的是 pingora 0.5。**现在是 0.8.1**，
升级的理由是 0.5 带着两个 CVSS 9.3 的请求走私漏洞（RUSTSEC-2026-0033 / -0034），在
多租户边缘上，请求走私意味着一个租户的请求被交给另一个租户的应用。真要按这份方案动手，
版本以 `crates/deis-proxy/Cargo.toml` 里那一行为准，别照抄下面这个。

```toml
[dependencies]
# 核心
pingora = "0.5"   # 已过时，见上；deis-proxy 现在是 0.8.1
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"

# WAF
regex = "1.10"
once_cell = "1.19"
dashmap = "6.1"
ipnetwork = "0.20"

# 配置
serde = { version = "1.0", features = ["derive"] }
toml = "0.8"

# 指标
prometheus = "0.13"

# 工具
anyhow = "1.0"
tracing = "0.1"
```

---

## 附录 B：配置示例

完整配置示例请参考：`examples/deis-proxy-waf-config.toml`

---

**文档所有者**: Deislet 团队
**审阅者**: [待定]
**批准者**: [待定]
**最后更新**: 2026-01-16
