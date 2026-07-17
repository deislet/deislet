# Deflux CLI 使用指南

## 概述

Deflux 提供了丰富的命令行参数支持，包括配置文件指定、配置测试、信号发送等功能。

## 基本用法

```bash
deflux [OPTIONS]
```

## 支持的选项

### 帮助信息
```bash
# 显示帮助信息
deflux -h
deflux -?
deflux --help
```

### 版本信息
```bash
# 显示版本信息
deflux -v
deflux --version
```

### 配置文件
```bash
# 指定配置文件
deflux -c /path/to/config.toml
deflux --config /path/to/config.toml

# 默认配置文件：config.toml
deflux
```

### 配置文件语法测试
```bash
# 测试配置文件语法
deflux -t
deflux -t -c /path/to/config.toml
deflux --test --config /path/to/config.toml
```

### 信号发送
```bash
# 发送信号到运行中的 Deflux 进程
deflux -s stop     # 快速强制关闭
deflux -s quit     # 优雅关闭，等待请求完成
deflux -s reload   # 热重载配置
deflux -s reopen   # 重新打开日志文件
```

## 信号详细说明

| 信号 | 作用描述 | 影响范围 | 适用场景 |
|------|----------|----------|----------|
| stop | 快速强制关闭 | 立即终止所有进程 | 紧急停止，快速关闭服务 |
| quit | 优雅关闭，等待请求处理完毕 | 等待请求完成后关闭 | 需要安全关闭服务 |
| reload | 热加载配置文件 | 启动新进程，优雅切换 | 配置变更后无中断生效 |
| reopen | 重新打开日志文件 | 不影响请求处理 | 日志轮转，日志文件切割 |

## 使用示例

### 基本启动
```bash
# 使用默认配置启动
deflux

# 使用指定配置启动
deflux -c /etc/deflux/config.toml
```

### 配置文件测试
```bash
# 测试默认配置文件
deflux -t

# 测试指定配置文件
deflux -t -c /etc/deflux/config.toml
```

### 进程管理
```bash
# 启动服务
deflux -c /etc/deflux/config.toml

# 在另一个终端中重载配置
deflux -s reload

# 优雅关闭服务
deflux -s quit
```

### 日志管理
```bash
# 重新打开日志文件（用于日志轮转）
deflux -s reopen
```

## 错误处理

CLI 会在以下情况返回错误：

1. **配置文件不存在**：`-t` 或 `-c` 指定的文件不存在
2. **配置文件语法错误**：`-t` 检测到 TOML 语法错误
3. **无效信号**：`-s` 指定了不支持的信号名称
4. **PID 文件不存在**：发送信号时找不到 PID 文件
5. **权限不足**：无法发送信号到目标进程
6. **位置参数错误**：提供了未知的位置参数

## 技术实现

CLI 模块位于 `src/cli.rs`，提供了：

- **CliConfig**: 配置结构体
- **CliAction**: 命令行动作枚举
- **SignalType**: 信号类型枚举
- **解析逻辑**: 命令行参数解析
- **执行逻辑**: 各种操作的具体实现

### 信号处理

在 Unix 系统上，信号映射如下：

- `stop` → `SIGTERM`
- `quit` → `SIGQUIT`
- `reload` → `SIGUSR1`
- `reopen` → `SIGUSR2`

## 注意事项

1. 信号发送功能仅在 Unix 系统上支持
2. PID 文件默认位置：`/var/run/deflux.pid` 或 `./deflux.pid`
3. 配置测试（`-t`）不会启动实际服务，仅验证语法
4. 帮助、版本、测试、信号操作会在执行后退出程序
5. 不支持位置参数，必须使用 `-c` 选项指定配置文件 