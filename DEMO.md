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

数据面先起，是因为它们不依赖控制面，而 `deis-runtime` 一启动就会往 `deis-telemetry`
投日志、往 `deis-store` / `deis-queue` 发调用。（目前只有 `deis-runtime` 在投日志，
其余组件还只写自己的 stdout。）

启动完成后可以这样打一发请求：

```bash
curl -H 'x-deis-app-id: demo' http://127.0.0.1:8086/hello/world
```

**8086 是节点的端口，不是 deis-proxy 的。** 演示里 deis-proxy 起着，但没有给它配
任何应用，所以它处在「透传」模式：一个上游（就是节点），头原样转发。走
`http://127.0.0.1:8087/hello/world` 也能通，只是它证明的是「pingora 在转发」，
不是「deis-proxy 选对了应用」。下面 `verify` 里发请求的各项全部直接打节点，原因也在这里
——要验的是控制面与节点这条线，让请求先绕一跳只会把失败点变多。

## 部署走的是真实路径

脚本没有任何后门，而且**不打控制面的 REST**：它敲的就是开发者自己会敲的命令。

1. `deis deploy`（在项目目录里跑）一条命令做完五件事：把
   `scripts/demo-assets/example-app/` 编成 `dist/deislet/bundle.zip`
   （`manifest.json` + `worker/index.js` + `assets/`）、建应用、建 `production`
   环境、把 zip 传成一个不可变版本、把环境指针指到那一版
2. `deis secret set DEMO_SECRET`（值从标准输入进去，不进 shell 历史、也不进别人的
   `ps`）存一条密钥：加密落库，只在下发那一刻解开。密钥不属于不可变的包，所以它不
   产生新版本，而是让控制面把**当前这一版**带着新环境重新下发一次
3. `deis deploy -e preview` 把改过的第二版部署到同一个应用的另一个环境

这里刻意不用 `curl` 打 API。`deis` 是开发者唯一拿得到的界面：演示要是绕过它，就只
证明了控制面是对的，而把唯一有人真正敲的那个入口留在没人验过的状态——第一个发现它
坏了的人，就会是第一个想用它的人。

每次部署之后控制面把**指针**推给每个节点（app / version / environment / bundle_url /
bundle_sha + 环境表），节点自己用 HTTP 把包拉下来、校验 sha256、解包、换 Isolate。
包本身从不进 gRPC 消息。`deis deploy` 打印这一版落到了哪些节点；没落到的节点会单独
红色列出来并让命令以非零退出——那台机器还在跑旧版本，这正是「绿色部署」最容易盖掉的
失败。

### 示例应用长什么样

```
scripts/demo-assets/example-app/
  .config.json                     声明服务（kv + queue）、队列 jobs、cron「每分钟」、
                                   密钥名 DEMO_SECRET、变量 BUILD_LABEL、资产目录
  public/index.html                静态资产，`/` 直接命中它，不起 Isolate
  public/css/app.css
  functions/hello/[name].ts        -> /hello/:name   占位符进 context.params
  functions/api/kv/[key].ts        -> /api/kv/:key   PUT 写、GET 读，经 deis-store
  functions/api/enqueue.ts         -> /api/enqueue   往 jobs 队列投一条消息
  functions/_queue.ts              队列消费函数（下划线开头 = 平台调用，不是路由）
  functions/_scheduled.ts          cron 处理函数，同上
```

`BUILD_LABEL` 是回滚那一项的抓手：v2 把它改成 `v2` 并顺手改了静态页面，回滚之后
两样要一起变回 `v1`。

## verify 检查什么

`./scripts/demo.sh verify` 分十一组跑十八项，每项打印它据以判断的证据：

| 组 | 检查 | 证明了什么 |
|---|---|---|
| CLI 部署 | `deis deploy` 的报告 + 打一发真请求 | 一条命令从源码走到对外服务：建应用、建环境、传版本、指针指过去，节点收下并真的在应答 |
| | `deis versions` | 两版都列出来了，且标出 production 跑的是哪一版——版本不会自称「当前」，是环境指向它 |
| 资产优先 | `GET /` | 返回的是包里那个文件本身，没起 Isolate；ETag 是资产自身的 sha256 |
| | 带 `If-None-Match` 再打一次 | 回 304 且零字节 |
| 路由 | `GET /hello/world` | 路由占位符进了 `context.params` |
| 密钥 | `deis secret list` | 只列出名字与最后修改时间；JSON 与人读两种输出都不含值，也没有任何命令能打印值 |
| | 同一响应的 `secretPresent` | 加密存库的密钥被解开、随下发进了 Isolate |
| | 递归 grep 数据目录与全部日志 | 解开后的明文哪儿都没落下 |
| 存储 | `PUT` 再 `GET /api/kv/<key>` | `ctx.services.kv` 真的走到了 deis-store（在它自己的库里能 grep 到这一行） |
| 队列 | `POST /api/enqueue` 后轮询 `/api/kv/job:<key>` | 消息进了 deis-queue，节点的轮询任务领到并交给 `_queue.ts` |
| cron | 轮询 `/api/kv/cron:last-run` | 控制面到点挑了**一个**节点触发，应用的 `_scheduled.ts` 真的跑了 |
| 环境 | 同一应用的 production 与 preview 各打一发 | 两个环境在同一台节点上并行，各返回各自版本的标签 |
| | 在一个环境写 KV，去另一个环境读 | 读不到——两个环境是两个存储租户 |
| 版本 | `deis deploy` v2 → `deis rollback` | 函数返回的标签和静态页面一起回到上一版。回滚不用报版本号：出事的人不该被要求先查一个号 |
| 观测 | `LogService/Query` | 应用自己 `console.log` 的那行能从 deis-telemetry 按应用查回来，并带上是哪个环境写的 |
| | `deis tail` 开流 → 之后才发一个请求 | 跟到的是**开流之后**产生的那一行（用一次性探针名字确认），不是历史里捞的 |
| 机队 | 控制面的 `/api/runtime` | 节点自己 `PUT .../registration` 登记上了；写在配置文件里的地址没有被节点自报的主机名覆盖掉 |
| | `deis status` | 节点是健康的、问得到的，而且它自己报出正在服务哪些环境——「目录说该跑什么」与「节点说在跑什么」是两回事 |

日志那一项需要 `grpcurl`，缺了会报为失败并说明原因，而不是悄悄跳过；其余各项除
`curl` 与 `jq` 外没有额外依赖。cron 那一项最长要等一个 tick 加一分钟，所以整体会跑
一两分钟。

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

顺序是先控制面与流量面、后数据面：`deis-runtime` 退出时可能还在往 `deis-telemetry`
冲日志、或者还有一笔存储调用没做完。最后一次请求的日志也是这样兜住的——`deis-runtime`
停机时会把缓冲区里剩下的日志再发一轮。

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
- **第一次跑之前要先构建控制面的 Web 面板**：`crates/deis-control/web/dist` 被
  gitignore 掉了，而 `deis-control` 在编译期把它嵌进二进制，所以少了它
  `cargo build` 直接失败。`demo.sh` 不会替你做这一步：
  `cd crates/deis-control/web && npm ci && npm run build`
- `verify` 里查日志那一项需要 `grpcurl`；缺了它这一项报失败，其余照跑
- `verify` 里发请求的各项都直接打节点端口，不经 deis-proxy（原因见上文）
- 节点这次是自己去连控制面的：`[control] endpoint` 一配，节点启动就调
  `GET /api/nodes/<id>/assignments`，把清单里每一版拉下来装好才报 healthy（原则 8）。
  节点也会向 `POST /api/nodes/<id>/register` 登记自己，用同一套凭据。演示里控制面
  配置仍然写着 `[[deis_runtime_nodes]]`：配置里列出来的节点不受登记过期影响，而
  「推」这个方向（新部署、改密钥当场送达）需要控制面知道这个节点在哪
- 存储地址不写在节点配置里：`store_endpoint` / `queue_endpoint` 配在控制面，随每次下发
  进环境表。节点在进 Isolate 之前把它们取走，所以应用的 `ctx.env` 里看不到，也无法用
  同名变量把存储客户端指到别处
- 若端口被占用，脚本会自动递增寻找可用端口
- `deis-store` 与 `deis-queue` 只有 gRPC 端口，探活只能确认「端口收得下连接」；
  `deis-telemetry` 另有一个 HTTP 抓取端口
- cron 那一项最长要等一个 tick 加一分钟（示例声明的是「每分钟」），所以 `verify`
  整体会跑一两分钟
