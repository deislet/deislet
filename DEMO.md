# 一键演示脚本

## 快速开始

在 `deislet/` 目录下执行：

```bash
./scripts/demo.sh start     # 起六个进程，并按真实路径部署示例应用
./scripts/demo.sh verify    # 逐项验证并打印证据
./scripts/demo.sh stop      # 停掉，等到每个进程真的退出
```

`start` 会自动：

- 选择可用端口，避免冲突（也会避开本机上别的用户占着的端口）
- 先编译一次全部二进制，再按依赖顺序拉起六个进程：
  - 数据面：`deis-telemetry`（gRPC + Prometheus 两个端口）、`deis-store`（KV/DB/Blob 三套 gRPC 服务共用一个端口）、`deis-queue`
  - 控制面与流量面：`deis-control`（单端口 HTTP/gRPC）、`deis-runtime`、`deis-proxy`
- 每个进程都探活之后才继续，探活同时看「端口有没有应答」和「进程还在不在」
- 按开发者会走的那条线部署示例应用（见下一节）
- 输出访问地址与日志路径

数据面先起，是因为它们不依赖控制面，而其它组件都会把日志打给 `deis-telemetry`。

启动完成后可以这样打一发请求：

```bash
curl -H 'x-deis-app-id: demo' http://127.0.0.1:8086/hello/world
```

## 部署走的是真实路径

脚本没有任何后门。它做的和一个人手工做的是同一串动作：

1. `deis build --vendor deislet` 把 `scripts/demo-assets/example-app/` 编成
   `dist/deislet/bundle.zip`（`manifest.json` + `worker/index.js` + `assets/`）
2. `POST /api/apps` 建应用
3. `POST /api/apps/demo/versions` 把 zip 传成一个不可变版本
4. `POST /api/apps/demo/environments` 建 `production` 环境
5. `PUT  .../secrets/DEMO_SECRET` 存一条密钥（加密落库，只在下发那一刻解开）
6. `POST .../deployments` 把环境指针指到那一版

第 6 步之后控制面把**指针**推给每个节点（app / version / environment / bundle_url /
bundle_sha + 环境表），节点自己用 HTTP 把包拉下来、校验 sha256、解包、换 Isolate。
包本身从不进 gRPC 消息。响应里的 `delivery` 段说明这一版落到了哪些节点、哪些没落到。

脚本还会额外构建并上传第二版（改了一个环境变量和那个静态页面），**不部署**，留给
`verify` 的回滚检查用。

### 示例应用长什么样

```
scripts/demo-assets/example-app/
  .config.json                     声明服务、队列、cron、密钥名、静态资产目录
  public/index.html                静态资产，`/` 直接命中它，不起 Isolate
  public/css/app.css
  functions/hello/[name].ts        -> /hello/:name   占位符进 context.params
  functions/api/kv/[key].ts        -> /api/kv/:key   PUT 写、GET 读，经 deis-store
  functions/api/enqueue.ts         -> /api/enqueue   往 jobs 队列投一条消息
  functions/_queue.ts              队列消费函数（下划线开头 = 平台调用，不是路由）
  functions/_scheduled.ts          cron 处理函数，同上
```

## verify 检查什么

`./scripts/demo.sh verify` 逐项跑，每项打印它据以判断的证据：

| 检查 | 证明了什么 |
|---|---|
| 静态资产 + ETag，带 `If-None-Match` 回 304 | 资产优先命中，且 ETag 是资产自身的 sha256 |
| `GET /hello/world` | 路由占位符进了 `context.params` |
| 同一响应的 `secretPresent` | 密钥下发到了 Isolate；同时递归 grep 数据目录与日志确认它没落盘 |
| `PUT` 再 `GET /api/kv/<key>` | `ctx.services.kv` 真的走到了 deis-store（并在它自己的库里能 grep 到） |
| `POST /api/enqueue` 后轮询 `/api/kv/job:<key>` | 消息进了 deis-queue，节点的轮询任务领到并交给消费函数 |
| 轮询 `/api/kv/cron:last-run` | 控制面到点挑了一个节点触发，应用的 `scheduled` 导出真的跑了 |
| 部署 v2 → 回滚 | 函数返回的标签和静态页面一起回到上一版 |
| `LogService/Query` | 应用自己 `console.log` 的那行能从 deis-telemetry 查回来 |

回滚那一项需要 `grpcurl` 之外的东西都没有额外依赖；日志那一项需要 `grpcurl`，缺了会
报为失败并说明原因，而不是悄悄跳过。

## 验证日志链路（手工）

示例应用每处理一次请求都会 `console.log` 一行。这行日志有两条去处：一条进节点自己
的 stdout（`.demo/logs/deis-runtime.log`），一条由 `deis-runtime` 的日志发送器攒成批
推给 `deis-telemetry`，落进它的库里。演示配置把发送间隔调到 1 秒，所以打完请求约一
秒后就能查回来：

```bash
# 端口用启动输出里那行「Telemetry (gRPC)」的值，默认 8092；被占用时脚本会自动往后挪
grpcurl -plaintext \
  -import-path crates/deis-telemetry/proto -proto telemetry.proto \
  -d '{"app_id":"demo","limit":5}' \
  127.0.0.1:8092 deis.telemetry.LogService/Query
```

查回来的每条记录里，`appId` 是 `demo`（应用维度，`deis tail` 与日志面板都按它过滤），
`component` 是 `deis-runtime`（哪个组件写的，不是哪台机器），机器写在 `fields.node`。

查不到的时候按这个顺序看：`.demo/logs/deis-runtime.log` 里有没有
「Log shipper started」那行——没有就是 `[logging]` 没配上；有的话再看有没有 warn 级
的「Could not ship a log batch」，那说明 `deis-telemetry` 那头没收着。

指标走同一个 `deis-telemetry`，从它的抓取端口看：

```bash
curl -s http://127.0.0.1:8093/metrics | grep deis_runtime_
```

## 常用命令

```bash
./scripts/demo.sh status
./scripts/demo.sh stop
```

`stop` 发的是 SIGTERM。六个组件收到后都会先把手上的请求做完再退出，所以信号是「请
你收尾」而不是「立刻停」——`deis-proxy` 通常比别的进程晚几秒才消失。脚本会逐个等到
进程真的没了才往下走，最后打一行「每个组件都已退出」；等超过 45 秒还没退的，脚本会
改发 SIGKILL 并说清楚这么做切断了什么。这一步不能省：进程还在，端口就还占着，下一
次 `start` 会绑不上一个刚被报告为「已停止」的服务。

顺序是先控制面与流量面、后数据面：上面几个退出时可能还在往 `deis-telemetry` 冲日志、
或者还有一笔存储调用没做完。最后一次请求的日志也是这样兜住的——`deis-runtime` 停机
时会把缓冲区里剩下的日志再发一轮。

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
  --demo-dir .demo \
  --app-dir scripts/demo-assets/example-app
```

## 运行目录

默认运行目录为 `.demo/`，包含：

- `deis-control.toml` / `deis-runtime.toml` / `deis-proxy.toml`
- `deis-store.toml` / `deis-queue.toml` / `deis-telemetry.toml`
- `secret.key`：加密环境密钥的主密钥（32 字节 hex，不进数据库、不进日志）
- `deis-control.db`、`bundles/`（控制面存的版本包）
- `deis-runtime-data/`：节点自己的库与它拉下来的包
- `deis-store-data/` / `deis-queue-data/` / `deis-telemetry-data/` 各自的数据目录
- `app-v1/` / `app-v2/`：示例应用的构建副本（构建会往项目里写 `dist/`，所以不在仓库里做）
- `logs/` 日志目录（含 `build.log`、每次构建的 `build-app-*.log`）
- `state.env` 运行状态

## 注意事项

- 需要本地安装 `cargo`、`curl`、`jq`，并确保 `ss`/`lsof`/`nc` 其一可用；
  `stop` 用 `timeout` 与 `tail --pid` 等进程退出（GNU coreutils）
- `verify` 里查日志那一项需要 `grpcurl`；缺了它这一项报失败，其余照跑
- 节点这次是自己去连控制面的：`[control] endpoint` 一配，节点启动就调
  `GET /api/nodes/<id>/assignments`，把清单里每一版拉下来装好才报 healthy（原则 8）。
  控制面配置里的 `[[deis_runtime_nodes]]` 仍然需要，那是「推」的方向——新部署、改密钥
  都靠它当场送到节点
- 存储地址不写在节点配置里：`store_endpoint` / `queue_endpoint` 配在控制面，随每次下发
  进环境表。节点在进 Isolate 之前把它们取走，所以应用的 `ctx.env` 里看不到，也无法用
  同名变量把存储客户端指到别处
- 若端口被占用，脚本会自动递增寻找可用端口
- `deis-store` 与 `deis-queue` 只有 gRPC 端口，探活只能确认「端口收得下连接」；
  `deis-telemetry` 另有一个 HTTP 抓取端口
- cron 那一项最长要等一个 tick 加一分钟（示例声明的是「每分钟」），所以 `verify`
  整体会跑一两分钟
