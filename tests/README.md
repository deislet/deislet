# Deislet E2E 测试套件

本目录包含 Deislet 项目群的完整端到端测试套件。

## 测试文件结构

### 集成测试 (Integration Tests)

#### `integration_workit_denira.rs`
- **功能**: 测试 Workit SDK ↔ Denira Supervisor 通信
- **覆盖**: 认证、注册、心跳、消息协议
- **运行**: `cargo run --bin integration_workit_denira`

#### `integration_denix_supervisor.rs`
- **功能**: 测试 Denix Runtime ↔ Denira Supervisor 完整生命周期
- **覆盖**: 连接、注册、心跳、状态同步、断线重连
- **运行**: `cargo run --bin integration_denix_supervisor`

#### `integration_deflux_denira.rs` ⭐ 新增
- **功能**: 测试 Deflux Load Balancer ↔ Denira Supervisor 通信
- **覆盖**: 连接、认证、注册、心跳、状态管理
- **运行**: `cargo run --bin integration_deflux_denira`

### E2E 测试脚本 (End-to-End Test Scripts)

#### `e2e_test.sh`
- **功能**: 基础 E2E 测试（原有）
- **覆盖**: 构建所有项目、启动 Denira/Denix、测试 App 生命周期
- **运行**: `./e2e_test.sh`

#### `e2e_new_features.sh` ⭐ 新增
- **功能**: 专门测试 Denira 的三大新功能
- **覆盖**:
  - 配置管理 API (Config Manager)
  - 命令执行 API (Command Executor)
  - 监控指标 API (Metrics Collector)
  - Prometheus 指标导出
  - WebSocket 实时推送
  - 数据库持久化
- **运行**: `./e2e_new_features.sh`
- **前提**: 需要先启动 Denira 或运行 `e2e_full.sh`

#### `e2e_full.sh` ⭐ 新增
- **功能**: 完整的平台 E2E 测试
- **覆盖**:
  - 并行构建所有项目 (Denira + Denix + Deflux)
  - 启动完整服务栈
  - 测试 Denira API (Workers、Heartbeats、Events)
  - 测试 Denix Runtime (App 部署、执行)
  - 测试 Deflux Load Balancer (连接、注册)
  - 测试监控指标 (Prometheus 格式)
  - 测试数据库持久化 (Workers、Events、Heartbeats 表)
  - 测试 Worker 心跳循环
- **运行**: `./e2e_full.sh`
- **特点**:
  - 自动清理环境
  - 完整的服务生命周期管理
  - 详细的测试统计和日志

#### `test_config_and_commands.sh` ⭐ 新增
- **功能**: 专门测试配置推送和命令执行功能
- **覆盖**:
  - 配置推送完整流程 (API → Denira → Worker)
  - 配置版本管理
  - 配置数据库持久化
  - 命令发送和响应 (GetStatus, Reload, etc.)
  - 命令状态追踪
  - 命令历史记录
  - Events API
- **运行**: `./test_config_and_commands.sh`
- **前提**: 需要先运行 `e2e_full.sh` 启动服务

## 运行测试

### 快速开始 - 完整测试

```bash
cd /mnt/sdb1/deislet/spec/tests

# 运行完整 E2E 测试套件（推荐）
./e2e_full.sh

# 测试完成后，在另一个终端测试配置和命令功能
./test_config_and_commands.sh
```

### 单独测试 Denira 新功能

```bash
# 1. 先启动 Denira
cd /mnt/sdb1/deislet/denira
cargo run --release -- --port 9001 --api-port 9002 --database /tmp/denira-test.db

# 2. 在另一个终端运行新功能测试
cd /mnt/sdb1/deislet/spec/tests
./e2e_new_features.sh
```

### 运行集成测试

```bash
cd /mnt/sdb1/deislet/spec/tests

# Workit SDK 集成测试
cargo run --bin integration_workit_denira

# Denix Runtime 集成测试
cargo run --bin integration_denix_supervisor

# Deflux Load Balancer 集成测试（新增）
cargo run --bin integration_deflux_denira
```

## 测试覆盖率

根据 `TEST_COVERAGE_ANALYSIS.md` 的分析：

### 当前覆盖情况

| 测试类型 | 覆盖率 | 状态 |
|---------|--------|------|
| 单元测试 | **85%** | ✅ 优秀 (125 个测试) |
| 集成测试 | **70%** | ✅ 良好 (现已包含 Deflux) |
| E2E 测试 | **75%** | ✅ 良好 (新增完整测试) |
| **总体** | **75-80%** | ✅ **已达标** |

### 各模块覆盖

#### Denira (Supervisor)
- ✅ 单元测试: 30 个
- ✅ 集成测试: 3 个 (Workit, Denix, Deflux)
- ✅ E2E 测试: 完整覆盖新功能
- **覆盖率: 80%**

#### Denix (Runtime)
- ✅ 单元测试: 26 个
- ✅ 集成测试: 2 个
- ✅ E2E 测试: 完整流程
- **覆盖率: 75%**

#### Deflux (Load Balancer)
- ✅ 单元测试: 33 个
- ✅ 集成测试: 1 个 (新增)
- ✅ E2E 测试: 包含在 e2e_full.sh
- **覆盖率: 70%**

#### Workit SDK
- ✅ 单元测试: 36 个
- ✅ 集成测试: 1 个
- **覆盖率: 80%**

## 测试环境要求

### 依赖工具

- Rust 工具链 (cargo, rustc)
- SQLite 3
- `jq` (JSON 处理)
- `nc` (netcat, 端口检查)
- `curl` (HTTP 请求)
- 可选: `websocat` (WebSocket 测试)

### 端口需求

确保以下端口未被占用：

- **9001**: Denira WebSocket (Workers 连接)
- **9002**: Denira HTTP API (管理 API)
- **8000**: Denix Runtime
- **8080**: Deflux Load Balancer

## 测试日志

E2E 测试运行后，可以查看以下日志文件：

```bash
# Denira 日志
tail -f /tmp/denira.log

# Denix 日志
tail -f /tmp/denix.log

# Deflux 日志
tail -f /tmp/deflux.log
```

## 临时文件清理

E2E 测试会创建临时数据库文件，测试完成后会自动清理：

- `/tmp/denira-test.db`
- `/tmp/denix-test.db`
- `/tmp/deflux-test.db`
- `/tmp/deflux-test.toml`

如需手动清理：

```bash
rm -f /tmp/denira*.{db,log} /tmp/denix*.{db,log} /tmp/deflux*.{db,log,toml}
```

## CI/CD 集成

在 CI 环境中运行测试：

```bash
#!/bin/bash
set -e

# 1. 运行所有单元测试
cargo test --workspace

# 2. 运行集成测试
cargo run --bin integration_workit_denira
cargo run --bin integration_denix_supervisor
cargo run --bin integration_deflux_denira

# 3. 运行完整 E2E 测试
cd spec/tests
./e2e_full.sh

# 4. 测试配置和命令功能
./test_config_and_commands.sh
```

## 故障排查

### E2E 测试失败常见原因

1. **端口被占用**
   ```bash
   # 检查端口占用
   sudo netstat -tlnp | grep -E '(9001|9002|8000|8080)'

   # 杀死占用进程
   sudo kill -9 <PID>
   ```

2. **数据库锁定**
   ```bash
   # 删除旧数据库
   rm -f /tmp/denira-test.db*
   ```

3. **Worker 连接超时**
   - 检查 Denira 日志: `tail /tmp/denira.log`
   - 确认 Denira 已成功启动
   - 检查认证密钥是否匹配

4. **API 请求失败**
   - 确认 Denira HTTP API 端口正确
   - 检查防火墙设置
   - 验证 curl 命令格式

## 贡献指南

添加新测试时：

1. **集成测试**: 在 `spec/tests/` 下创建 `integration_*.rs` 文件
2. **E2E 脚本**: 创建 `.sh` 脚本并设置可执行权限 (`chmod +x`)
3. **更新文档**: 在本 README 中添加测试说明
4. **测试验证**: 本地运行通过后再提交

## 测试覆盖报告

详细的测试覆盖分析请参考：

- `TEST_COVERAGE_ANALYSIS.md` - 完整覆盖率分析报告
- `ARCHITECTURE.md` - 系统架构和设计文档

## 许可证

Apache 2.0
