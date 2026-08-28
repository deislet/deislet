# 一键演示脚本

## 快速开始

在 `deislet/` 目录下执行：

```bash
./scripts/demo.sh start
```

脚本会自动：

- 选择可用端口，避免冲突
- 启动 `deis-control`（单端口 HTTP/gRPC）、`deis-runtime`、`deis-proxy`
- 自动部署 `deis-runtime` 示例应用
- 输出访问地址与日志路径

## 常用命令

```bash
./scripts/demo.sh status
./scripts/demo.sh stop
```

## 可选参数

```bash
./scripts/demo.sh start   --deis-control-port 8085   --deis-runtime-port 8086   --deis-proxy-port 8087   --secret <64位hex>   --demo-dir .demo
```

## 运行目录

默认运行目录为 `.demo/`，包含：

- `deis-control.toml` / `deis-runtime.toml` / `deis-proxy.toml`
- `deis-control.db`
- `logs/` 日志目录
- `state.env` 运行状态

## 注意事项

- 需要本地安装 `cargo`、`curl`、`jq`，并确保 `ss`/`lsof`/`nc` 其一可用
- `deis-runtime` 会自动连接 `deis-control`，断线后持续重试
- 若端口被占用，脚本会自动递增寻找可用端口
