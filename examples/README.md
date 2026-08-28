# Pingora 参考项目

本目录收集了基于 Cloudflare Pingora 框架的优秀开源项目，用于参考学习和功能借鉴。

## 项目列表

### 1. [pingora](./pingora/) - Cloudflare 官方框架
**仓库**: https://github.com/cloudflare/pingora
**用途**: Pingora 官方框架，用于构建快速、可靠、可演化的网络服务
**参考价值**:
- 官方文档和示例
- 核心 API 和最佳实践
- Rate Limiter、Filter、Health Check 等核心功能实现

**关键目录**:
- `docs/user_guide/` - 用户指南文档
- `pingora-proxy/examples/` - 代理示例
- `pingora-limits/` - 限流实现
- `pingora-load-balancing/` - 负载均衡

---

### 2. [pingora-waf](./pingora-waf/) - Web 应用防火墙
**仓库**: https://github.com/AarambhDevHub/pingora-waf
**用途**: 基于 Pingora 的高性能 WAF，专注于 Web 安全防护
**特性**:
- SQL 注入和 XSS 攻击防护
- 15K+ req/sec 吞吐量，<7ms 延迟
- 热配置重载
- Prometheus 指标和 Grafana 仪表盘

**参考价值**:
- WAF 规则引擎实现
- 请求检测和威胁识别逻辑
- 安全日志和指标收集
- 生产环境配置示例

**关键文件**:
- `src/waf/` - WAF 核心逻辑
- `src/rules/` - 攻击检测规则
- `config/waf.yaml` - 配置示例

---

### 3. [pingap](./pingap/) - 全功能反向代理
**仓库**: https://github.com/vicanso/pingap
**用途**: 类似 Nginx 的反向代理，功能丰富、配置简单
**特性**:
- 认证：JWT、Key Auth
- 安全：CSRF 防护、IP/Referer/UA 限制
- 流量控制：限流、缓存
- 内容修改：重定向、请求/响应修改
- 20+ 开箱即用的插件

**参考价值**:
- 插件架构设计
- 多种认证机制实现
- 动态配置管理
- Web UI 管理界面

**关键目录**:
- `src/plugin/` - 插件实现
- `src/proxy/` - 代理核心
- `web/` - Web 管理界面

---

### 4. [aralez](./aralez/) - 轻量级反向代理
**仓库**: https://github.com/sadoyan/aralez
**用途**: 基于 Pingora 的反向代理，专注于性能和简洁
**特性**:
- 内置限流器（按虚拟主机和全局）
- 健康检查
- 简单配置

**参考价值**:
- 简洁的代码结构
- 限流器实现
- 虚拟主机管理

**关键文件**:
- `src/proxy/` - 代理实现
- `src/rate_limiter/` - 限流逻辑

---

### 5. [pingora-proxy-manager](./pingora-proxy-manager/) - 代理管理平台
**仓库**: https://github.com/DDULDDUCK/pingora-proxy-manager
**用途**: 零停机反向代理管理器，带 Web UI
**特性**:
- Web 管理界面
- IP 白名单/黑名单
- Basic 认证
- 通配符 SSL
- TCP/UDP 流支持
- 零停机热重载

**参考价值**:
- Web UI 设计（我们之前参考过）
- 配置管理和存储
- 零停机重载机制
- SSL/TLS 配置

**关键目录**:
- `web/` - React Web UI
- `src/config/` - 配置管理
- `src/ssl/` - SSL/TLS 处理

---

## 使用建议

### 为 Control/Proxy 添加功能时的参考

1. **WAF 功能** → 参考 `pingora-waf`
   - SQL 注入、XSS 检测规则
   - 威胁日志和指标

2. **认证和安全** → 参考 `pingap`
   - JWT/API Key 认证
   - IP 黑白名单
   - CSRF 防护

3. **限流** → 参考 `aralez` 和官方 `pingora/pingora-limits/`
   - 按虚拟主机限流
   - 全局限流
   - Token Bucket 算法

4. **Web UI** → 参考 `pingora-proxy-manager` 和 `pingap`
   - 配置管理界面
   - 实时监控面板

5. **零停机重载** → 参考 `pingora-proxy-manager`
   - 配置热重载机制
   - 优雅关闭

6. **插件系统** → 参考 `pingap`
   - 插件架构设计
   - 请求/响应过滤器链

---

## 目录维护

- 这些项目仅用于学习和参考
- 定期更新（`git pull`）以获取最新代码
- 不要直接修改这些项目的代码，需要时复制到我们的代码库

---

## 相关文档

- [Pingora 官方文档](https://github.com/cloudflare/pingora/tree/main/docs)
- [Pingora 用户指南](https://github.com/cloudflare/pingora/tree/main/docs/user_guide)
- [Rate Limiter 指南](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/rate_limiter.md)
- [Request Filter 指南](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/modify_filter.md)
