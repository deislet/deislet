# 机器通道迁移之后仍然存在的共享密钥

> 状态：待办清单，不是决策记录。决策见 [ADR-025](../adr/025-machine-identity-and-capabilities.md)，
> 本轮的实施过程见[阶段分析](phase-analysis-2026-09-03.zh.md)。

## 这份文档为什么存在

`node_preshared_keys`（舰队级共享口令）正在被每实例 mTLS 身份替换。那件事做完之后，
**节点与控制面之间**不再有共享口令 —— 但这套安装**仍然在用别的全场通用密钥**。

把这条写下来，是因为它极易被读成更强的结论。「舰队口令没有了」和「共享密钥模式结束了」
差着好几项工作，而后者是一个会让人停止加固的句子。任何对外描述安全态势的文字，
都必须同时说出下面这张表还没清空。

## 还在的东西

| 凭证 | 在哪里 | 它开的是什么门 | 现在的形态 |
|---|---|---|---|
| `auth_secret`（deis-store） | `crates/deis-store/src/config.rs` | 节点对对象与 KV 存储的全部调用 | 全场一个值 |
| `auth_secret`（deis-queue） | `crates/deis-queue/src/config.rs` | 队列的投递、领取与 fence 操作 | 全场一个值 |
| `auth_secret`（deis-telemetry） | `crates/deis-telemetry/src/config.rs` | 日志、trace 与指标的写入 | 全场一个值 |
| storage token | `deis_wire::conventions::storage_token`（HMAC-SHA256 派生） | 一个租户在存储上的读写 | 由上面的 `storage_auth_secret` 按租户派生；不过期、不可单独撤销，轮换等于一次性作废全部 |
| `migration_admin_token` | `crates/deis-control/src/config.rs` 与 `deis-backend-migration` | 后端迁移与恢复的管理操作 | 独立的静态 token |
| inspector 会话令牌 | `crates/deis-runtime/src/app/manager.rs` | 一个调试会话 | **不属于这一类**：节点自己铸造、驻留内存、会过期，与舰队无关。列在这里只是为了说明它为什么不该跟着一起删 |

## 为什么这一轮不做

ADR-025 把这些划在 stage 5（data capability enforcement），而本轮的范围是节点与控制面之间的
机器通道。硬把它们塞进来会同时改动三个额外 crate、三个 systemd 单元和存储契约，
使这一批改动无法被单独评审、单独回滚。

## 做的时候要注意什么

- **storage token 不是「再加一把钥匙」的问题**。它是从一个共享值 HMAC 派生的，所以它的
  「按租户隔离」是真的（一个租户的 token 对另一个租户无用），但**撤销粒度是全部**：
  换掉那个值，每个租户的 token 同时作废。真正的修法是短期、受众限定、绑定持有密钥的能力券
  （ADR-025 M2），不是把这个值换成另一个值。
- **三个 `auth_secret` 是三条独立的门**，不要合并成一个「服务密钥」再共享 —— 那只是把三个
  全场通用值换成一个，范围更大。
- 这三个服务各自的监听同样需要「谁在调用」的答案，而 `deis-identity` 已经能提供：它们要的是
  同一套 `MachineIdentity` 与 SPIFFE 命名，不需要新机制。
- 动它们之前先看 `docs/adr/025-machine-identity-and-capabilities.md` 的非目标一节：
  那里明确禁止为 legacy 凭证发「按节点伪造的衍生 token」，这类捷径在这几处尤其诱人。

---

## 附：一条与共享密钥无关、但同轮查实的运行缺陷

> 放在这里是因为它没有别的归处，而它是靠一次事故发现的。**已查实，不是推测**；
> 修复需要改动部署状态机，那是产品决策，见末尾的三个待定问题。

### 现象

2026-09-11 的一次门禁运行中，一个额外注册的节点在部署投递期间因注册过期被移出舰队。此后
该应用 production 环境的**每一次部署都返回 409「deployment plan ... is already in progress」**，
跨三次独立的 verify 运行、超过 30 分钟没有自行解开。同一份代码在节点全程在位的干净运行里 97 项全过。

### 查实的机制

- **409 的出处**：`catalog/store.rs` 的 `create_deployment_plan` 在写事务里拒绝「该环境已有
  计划处于非终态」。非终态有六个，终态只有 `active` / `failed` / `aborted` 三个。
- **target 集合在建计划时冻结，并且故意把失联节点算进去**（`delivery/mod.rs` 的
  `serving_target_ids`）—— 这是既定设计，为的是让回执能点名「这台没收到」。
- **离队 target 的推送永远是失败，不是跳过**：deliverer 在**拨号之前**就把 departed / contested
  的 target 翻译成 `Transport` 错误。而判定成功的谓词是全称的（`failed` 必须为空）。
- **两条永久卡死路径**：pre-commit 卡在 `aborting`（补偿推不完就不迁移），post-commit 卡在
  `activating`/`reconciling`/`degraded` —— 后者**唯一的出口是 `active`**，而 `active` 要求每个
  冻结 target 都有 `active` 观察记录，观察记录只在协调者推送成功时写入。两条路径对外表现相同。
- **协调者租约帮不上忙**：它保护的是「协调者进程死了」。本案里协调者每轮都正常跑完并主动释放，
  两秒后又被重新认领 —— 接管机制工作得很好，它每两秒忠实地把同一个注定失败的动作再做一遍。
  **没有尝试上限、没有退避、没有截止时间。**
- **reaper 不管部署计划**，从来没打算管。把节点移出舰队的是节点健康轮询，它不查、不通知、不释放任何计划。

**唯一的自愈条件是那个节点用同一个 id 重新注册。** 真正重启的机器会自愈；一个再也不会回来的
id 不会。运维手上唯一的杠杆 `DELETE .../registration` 反而把失败从 `Transport` 变成
`UnknownNode`，依然是失败，解不开。

### 一条已排除的怀疑

失败与本轮的 mTLS 改动无关。那个幽灵节点是用证书注册、未上报 mTLS 管理地址的，所以回落到
「第一把钥匙 + 明文地址」，而那个地址正是真实节点的管理面。**它在册期间推送本应成功**；
266 次失败全部发生在它被判定离队之后，是上面那条「离队即失败」的既有行为。

### 修的时候要守住什么

「暂时联系不上」和「已被判定离开舰队」是两件事，**不能混**：
- 联系不上但还在名单上 —— 继续等是对的，现行行为不要动。
- 已经离队 —— 无限等是错的。但「一离队就放弃」会犯反向错误：默认注册 TTL 是 90 秒，
  一台机器重启 91 秒就会离队。所以判据必须是**离队 + 一个宽裕的截止时间**，不是二者之一。
- 放弃时**不能假装那个 target 确认过**。必须留下「这个 target 从未被对账」的持久证据，
  否则下一次事故里没人能解释那台机器上为什么有一个陌生的 generation。

### 附带发现

`deployments.rs` 里 `let _ = abort_prepared_plan(...)` 丢掉了返回值。补偿没做完时 API 依然回
一个干净的 503「production 指针未改变」，调用方**完全看不出还有一个计划留在 `aborting` 里占着
环境**。这就是为什么这个卡死要到「下一次部署」才暴露。

### 三个待定的产品决策

1. 观察到的那次究竟卡在 `aborting` 还是 `reconciling`？两者 409 相同、都同样永久，**修法不同**
   （前者补完 `aborting→failed`，后者需要新增一个诚实的 post-commit 终态）。
2. 把 post-commit 计划从 409 的阻塞名单里摘掉是否安全？一个被取代的旧计划，其 reconcile 循环
   仍在向掉队节点推它自己的 routing generation —— 若那台节点在新计划提交后才回来，可能先收到旧的。
3. 截止时间取多少？硬约束是必须明显大于 `node_registration_ttl_secs`（默认 90 秒）加一次正常重启，
   具体长度是运维对「一台机器多久没回来算真没了」的判断，应当可配置。

### 同轮观测到的一条较轻的脆弱性

`scripts/demo.sh` 的 `check_dynamic_node_registration` 一旦在注册与注销之间中断，会把一个幽灵
节点留在舰队上，让此后所有部署报「部分失败」。门禁运行被中断的代价因此不止是「这次没跑完」——
它会把环境留在让后续运行都失真的状态里。
