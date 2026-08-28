# 一键演示脚本

## 快速开始

在 `deislet/` 目录下执行：

```bash
./scripts/demo.sh start
```

脚本会自动：

- 选择可用端口，避免冲突（也会避开本机上别的用户占着的端口）
- 先编译一次全部二进制，再按依赖顺序拉起六个进程：
  - 数据面：`deis-telemetry`（gRPC + Prometheus 两个端口）、`deis-store`（KV/DB/Blob 三套 gRPC 服务共用一个端口）、`deis-queue`
  - 控制面与流量面：`deis-control`（单端口 HTTP/gRPC）、`deis-runtime`、`deis-proxy`
- 每个进程都探活之后才继续，探活同时看「端口有没有应答」和「进程还在不在」
- 自动部署 `deis-runtime` 示例应用
- 输出访问地址与日志路径

数据面先起，是因为它们不依赖控制面，而其它组件都会把日志打给 `deis-telemetry`。

启动完成后可以这样打一发请求：

```bash
curl -H 'x-deis-app-id: demo' http://127.0.0.1:8087/hello
```

## 常用命令

```bash
./scripts/demo.sh status
./scripts/demo.sh stop
```

`stop` 发的是 SIGTERM。`deis-store` / `deis-queue` / `deis-telemetry` / `deis-proxy`
收到后会先把手上的请求做完再退出；`deis-control` 与 `deis-runtime` 目前还没有停机
处理，属于直接退出。`deis-proxy` 有几秒的优雅停机窗口，所以它比别的进程晚一点消失。

## 可选参数

```bash
./scripts/demo.sh start \
  --deis-control-port 8085 \
  --deis-runtime-port 8086 \
  --deis-proxy-port 8087 \
  --deis-store-port 8090 \
  --deis-queue-port 8091 \
  --deis-telemetry-port 8092 \
  --deis-telemetry-metrics-port 8093 \
  --secret <64位hex> \
  --demo-dir .demo
```

## 运行目录

默认运行目录为 `.demo/`，包含：

- `deis-control.toml` / `deis-runtime.toml` / `deis-proxy.toml`
- `deis-store.toml` / `deis-queue.toml` / `deis-telemetry.toml`
- `deis-control.db`
- `deis-store-data/` / `deis-queue-data/` / `deis-telemetry-data/` 各自的数据目录
- `logs/` 日志目录（含 `build.log`）
- `state.env` 运行状态

## 注意事项

- 需要本地安装 `cargo`、`curl`、`jq`，并确保 `ss`/`lsof`/`nc` 其一可用
- `deis-runtime` 会自动连接 `deis-control`，断线后持续重试
- 若端口被占用，脚本会自动递增寻找可用端口
- `deis-store` 与 `deis-queue` 只有 gRPC 端口，探活只能确认「端口收得下连接」；
  `deis-telemetry` 另有一个 HTTP 抓取端口，`curl http://127.0.0.1:<metrics 端口>/metrics`
  能直接看到它自己的计数
- 目前 `deis-runtime` 还没有接上 `deis-store` / `deis-queue` / `deis-telemetry`，
  这三个进程起来了但还没有流量经过；接线是后续工作
