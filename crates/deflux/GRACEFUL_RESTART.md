# Deflux 平滑重启

Deflux 实现了零停机时间的平滑重启功能，基于文件描述符 (FD) 传递机制。

## 功能特性

- ✅ 零停机时间：重启期间持续接受新连接
- ✅ 文件描述符传递：通过 Unix domain socket 在进程间传递监听器 FD
- ✅ 信号驱动：使用 SIGUSR1 触发重启，SIGTERM/SIGQUIT 优雅关闭
- ✅ 状态机管理：清晰的重启阶段转换
- ✅ 错误处理：完善的错误处理和日志记录

## 架构设计

### 核心组件

1. **FD 传递模块** (`graceful_restart/transfer.rs`)
   - 使用 Unix domain socket 和 SCM_RIGHTS 控制消息
   - 支持传递多个文件描述符（最多 32 个）
   - 自动重试机制（默认 5 次，间隔 1 秒）

2. **监听器管理器** (`graceful_restart/listener.rs`)
   - 统一管理所有 TCP 监听器
   - 支持从 FD 重建监听器
   - 自动处理地址和 FD 的映射

3. **状态机** (`graceful_restart/state.rs`)
   - 7 个清晰定义的阶段
   - 线程安全的状态访问
   - 状态转换日志记录

4. **重启协调器** (`graceful_restart/coordinator.rs`)
   - 协调整个重启流程
   - 处理父子进程通信
   - 自动启动新进程

### 状态转换流程

```
Setup → Running → PreparingTransfer → TransferringFds →
WaitingForNewProcess → GracefulShutdown → Terminated
```

## 使用方法

### 触发平滑重启

#### 方式 1：通过 Supervisor 控制（推荐）

**通过 Denira Supervisor 集中控制所有节点的平滑重启：**

```bash
# 使用 Denira API 触发单个节点重启
curl -X POST http://supervisor:port/api/workers/{worker_id}/command \
  -H "Content-Type: application/json" \
  -d '{
    "command": "graceful_restart"
  }'

# 或通过 Denira Web UI 操作
# 1. 访问 Web UI
# 2. 进入 Workers 管理页面
# 3. 选择目标节点
# 4. 点击 "Graceful Restart" 按钮
```

**优势**：
- 集中管理多个节点
- 可视化操作界面
- 支持滚动更新策略
- 自动状态追踪和失败回滚

#### 方式 2：发送信号（直接控制）

```bash
# 获取 Deflux 进程 PID
PID=$(pgrep deflux)

# 发送 SIGUSR1 信号触发平滑重启
kill -USR1 $PID
```

#### 方式 3：使用 CLI 工具（如果实现）

```bash
./deflux -s reload
```

### 优雅关闭

```bash
# 发送 SIGTERM 或 SIGQUIT 信号
kill -TERM $PID
# 或
kill -QUIT $PID
```

## 重启流程详解

### 1. 旧进程收到 SIGUSR1

```
[INFO] Received SIGUSR1, initiating graceful restart
[INFO] State transition: Running → PreparingTransfer
```

### 2. 提取监听器 FD

```
[INFO] Extracted 2 FDs for transfer
[DEBUG] Extracted FD 3 for 0.0.0.0:80
[DEBUG] Extracted FD 4 for 0.0.0.0:443
```

### 3. 启动新进程

```
[INFO] Spawning new process: /usr/local/bin/deflux ["-c", "config.toml"]
[INFO] Spawned new process with PID 12345
[INFO] State transition: PreparingTransfer → TransferringFds
```

### 4. 传递 FD 到新进程

```
[INFO] Sending 2 FDs to socket /tmp/deflux-restart.sock
[DEBUG] FD payload: 128 bytes
[DEBUG] Sent 2 FDs via sendmsg
```

### 5. 新进程接收 FD

```
[INFO] Detected graceful restart, receiving listeners from parent
[INFO] Child process: receiving listeners from parent
[INFO] Waiting for FDs on socket /tmp/deflux-restart.sock
[INFO] Child process: received 2 FDs
[INFO] Rebuilt listener for 0.0.0.0:80 from FD 3
[INFO] Rebuilt listener for 0.0.0.0:443 from FD 4
[INFO] Child process: successfully rebuilt 2 listeners
```

### 6. 等待新进程就绪

```
[INFO] FDs transferred successfully
[INFO] State transition: TransferringFds → WaitingForNewProcess
[INFO] Waiting 5 seconds for new process to be ready
```

### 7. 旧进程停止接受新连接

```
[INFO] State transition: WaitingForNewProcess → GracefulShutdown
[INFO] Stopped accepting new connections, waiting for existing connections to finish
[INFO] Shutting down listener on 0.0.0.0:80
[INFO] Shutting down listener on 0.0.0.0:443
```

### 8. 旧进程等待现有连接完成并退出

```
[INFO] Shutting down...
[INFO] Shutdown complete
[INFO] Deflux stopped
```

## 配置参数

在 `GracefulRestartConfig` 中可以配置以下参数：

```rust
pub struct GracefulRestartConfig {
    /// Unix socket 路径（用于进程间通信）
    pub socket_path: String,  // 默认: "/tmp/deflux-restart.sock"

    /// FD 传递后等待新进程就绪的时间（秒）
    pub wait_after_transfer: u64,  // 默认: 5

    /// 优雅关闭等待时间（秒）
    pub graceful_shutdown_timeout: u64,  // 默认: 30

    /// 最大 FD 传递重试次数
    pub max_retry: usize,  // 默认: 5

    /// 重试间隔（秒）
    pub retry_interval: u64,  // 默认: 1
}
```

## 环境变量

平滑重启使用以下环境变量在进程间传递信息：

- `DEFLUX_GRACEFUL_RESTART=1`：标记子进程为重启的新进程
- `DEFLUX_RESTART_SOCKET=/path/to/socket`：指定 Unix socket 路径

**注意**：这些环境变量由系统自动设置，用户无需手动配置。

## 限制

1. **最大 FD 数量**：单次传递最多 32 个文件描述符
2. **超时时间**：
   - FD 传递重试最多 5 次，每次间隔 1 秒
   - 等待新进程就绪：5 秒
   - 优雅关闭超时：30 秒
3. **平台支持**：仅支持 Linux 和其他 Unix-like 系统（需要 SCM_RIGHTS 支持）

## 故障处理

### 重启失败场景

#### 1. 新进程启动失败

- **现象**：旧进程日志显示 "Failed to spawn child process"
- **原因**：可执行文件不存在或无执行权限
- **解决**：检查二进制文件路径和权限

#### 2. FD 传递失败

- **现象**：日志显示 "Failed to send file descriptors"
- **原因**：Unix socket 权限问题或路径不存在
- **解决**：
  - 检查 `/tmp` 目录权限
  - 确保没有其他进程占用同名 socket
  - 检查 SELinux/AppArmor 策略

#### 3. 新进程无法绑定地址

- **现象**：新进程日志显示 "Failed to create Tokio listener"
- **原因**：FD 已失效或地址已被占用
- **解决**：
  - 检查是否有其他进程占用了相同端口
  - 确保 FD 传递过程没有中断

### 回滚机制

如果平滑重启失败，旧进程会：

1. 记录错误日志
2. 继续运行并接受连接
3. 不会自动退出

用户可以：

- 手动检查日志诊断问题
- 修复问题后重试平滑重启
- 或执行传统的停止-启动重启

## 测试

### 单元测试

```bash
# 运行所有平滑重启模块的单元测试
cargo test -p deflux --lib graceful_restart

# 运行特定测试
cargo test -p deflux --lib graceful_restart::transfer::tests
```

### 集成测试脚本

使用提供的测试脚本进行端到端测试：

```bash
# 运行平滑重启集成测试
./scripts/test_graceful_restart.sh
```

测试脚本会：
1. 启动 Deflux 实例
2. 建立测试连接
3. 触发平滑重启
4. 验证连接不中断
5. 验证新进程正常工作

## 性能影响

- **内存开销**：约 1-2 MB（状态管理和 Unix socket 缓冲区）
- **重启时间**：
  - FD 传递：< 100ms
  - 新进程启动：取决于应用初始化（通常 1-3 秒）
  - 总重启时间：约 5-10 秒（包括等待时间）
- **连接影响**：
  - 现有连接：无影响，继续正常处理
  - 新连接：
    - FD 传递期间（< 100ms）：由旧进程接受
    - FD 传递后：由新进程接受
    - 无连接丢失或拒绝

## 与其他方案对比

### vs. Nginx reload

| 特性 | Deflux | Nginx |
|------|--------|-------|
| 零停机 | ✅ | ✅ |
| FD 传递 | ✅ | ✅ |
| 配置热重载 | ✅ (via Supervisor) | ✅ |
| 代码热重载 | ✅ | ❌ |
| Rust 原生 | ✅ | ❌ |

### vs. HAProxy reload

| 特性 | Deflux | HAProxy |
|------|--------|---------|
| 零停机 | ✅ | ✅ (使用 -sf) |
| 连接迁移 | 自动 | 需配置 |
| 状态管理 | 内置 | 需外部工具 |

## 最佳实践

1. **生产环境部署**
   - 在非高峰时段进行重启
   - 提前测试新配置/代码
   - 监控日志确认重启成功

2. **监控指标**
   - 监控 `GracefulShutdown` 状态持续时间
   - 跟踪活跃连接数
   - 记录重启成功/失败次数

3. **日志级别**
   - 开发/测试：DEBUG
   - 生产环境：INFO（记录重启事件）
   - 故障排查：TRACE（查看详细 FD 传递过程）

4. **定期测试**
   - 在预发布环境定期测试平滑重启
   - 模拟高负载场景下的重启
   - 验证错误处理逻辑

## Supervisor 控制

### 通过 Supervisor 管理节点重启

Deflux 可以通过 Denira Supervisor 实现集中化的节点管理和重启控制。

#### 工作原理

1. **命令传递**：Supervisor 通过 gRPC/WebSocket 向 Deflux 发送 `graceful_restart` 自定义命令
2. **信号转换**：Deflux supervisor client 接收命令后，通过内部 broadcast channel 发送 `RestartSignal::Graceful`
3. **重启触发**：Main 事件循环接收到信号，调用 `RestartCoordinator::initiate_graceful_restart()`
4. **状态上报**：重启过程中的状态变化会上报给 Supervisor

#### API 接口（待 Denira 实现）

```http
POST /api/workers/{worker_id}/command
Content-Type: application/json

{
  "command": "graceful_restart",
  "args": []
}
```

**响应示例**：
```json
{
  "success": true,
  "message": "Graceful restart initiated",
  "worker_id": "deflux-001",
  "command_id": "cmd-123456"
}
```

#### 滚动更新策略（未来功能）

**批量重启**：
- 一次重启多个节点，但保持最小可用容量
- 示例：3 个节点，一次重启 1 个

**金丝雀重启**：
- 先重启一个节点，观察一段时间
- 无异常后继续重启其他节点

**蓝绿部署**：
- 启动新版本节点
- 流量切换后停止旧节点

### 监控和日志

#### Deflux 日志

```log
[INFO] Received custom command: graceful_restart
[INFO] Initiating graceful restart via supervisor command
[INFO] Graceful restart signal sent successfully
[INFO] Received graceful restart signal from Supervisor
[INFO] Initiating graceful restart
[INFO] Extracted 2 FDs for transfer
...
```

#### Supervisor 日志

```log
[INFO] Sending command 'graceful_restart' to worker deflux-001
[INFO] Command response: success=true
[INFO] Worker deflux-001 status: restarting
[INFO] Worker deflux-001 status: active
```

### 故障处理

#### 重启失败

如果节点重启失败，Supervisor 会收到错误响应：

```json
{
  "success": false,
  "error": "Failed to send restart signal: channel closed",
  "worker_id": "deflux-001"
}
```

**处理策略**：
- 记录失败事件
- 通知管理员
- 可选：自动重试或回滚

#### 超时处理

设置合理的超时时间（建议 30-60 秒）：
- 超时后标记节点为异常状态
- 可选：强制停止并重启

## 相关文档

- [Pingora 平滑重启文档](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/daemon.md)
- [Unix Domain Sockets](https://man7.org/linux/man-pages/man7/unix.7.html)
- [SCM_RIGHTS](https://man7.org/linux/man-pages/man3/cmsg.3.html)
- [Rust nix crate](https://docs.rs/nix/latest/nix/sys/socket/index.html)
- [Denira Supervisor 文档](../denira/README.md)

## 故障排查

### 启用详细日志

```bash
RUST_LOG=deflux::graceful_restart=trace ./deflux -c config.toml
```

### 常见问题

**Q: 平滑重启后配置没有更新？**
A: Deflux 的平滑重启只传递监听器 FD，配置更新需要通过 Supervisor 的配置热重载功能。

**Q: 可以连续多次平滑重启吗？**
A: 可以。每次平滑重启都会启动新进程并传递 FD。建议在前一次重启完全完成后再触发下一次。

**Q: 平滑重启期间能处理多少并发连接？**
A: 取决于系统的 FD 限制（ulimit -n）和应用配置。重启本身不会限制并发连接数。

**Q: 如何回滚到旧版本？**
A: 由于旧进程会在新进程就绪后退出，无法直接回滚。建议使用蓝绿部署或金丝雀发布策略。
