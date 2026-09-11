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
