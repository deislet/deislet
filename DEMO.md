# 一键演示脚本

## 快速开始

在 `deislet/` 目录下执行：

```bash
./scripts/demo.sh start
```

脚本会自动：

- 选择可用端口，避免冲突
- 启动 `denira`（单端口 HTTP/gRPC）、`denix`、`deflux`
- 自动部署 `denix` 示例应用
- 输出访问地址与日志路径

## 常用命令

```bash
./scripts/demo.sh status
./scripts/demo.sh stop
```

## 可选参数

```bash
./scripts/demo.sh start   --denira-port 8085   --denix-port 8086   --deflux-port 8087   --secret <64位hex>   --demo-dir .demo
```

## 运行目录

默认运行目录为 `.demo/`，包含：

- `denira.toml` / `denix.toml` / `deflux.toml`
- `denira.db`
- `logs/` 日志目录
- `state.env` 运行状态

## 注意事项

- 需要本地安装 `cargo`、`curl`、`jq`，并确保 `ss`/`lsof`/`nc` 其一可用
- `denix` 会自动连接 `denira`，断线后持续重试
- 若端口被占用，脚本会自动递增寻找可用端口
