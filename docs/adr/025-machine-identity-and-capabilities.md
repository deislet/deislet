# ADR-025：每实例机器身份、独立管理通道与持有证明能力

- 状态：Accepted
- 日期：2026-09-05
- 决策者：产品负责人已确认的默认安全、统一标准与产品级推进方向
- 问题来源：[阶段分析](../archive/phase-analysis-2026-09-03.zh.md)
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)
- 关联：[ADR-009](009-deployment-activation.md)、[ADR-013](013-secure-by-default.md)、[ADR-019](019-coordinated-data-lifecycle.md)、[ADR-023](023-backend-migration-fence-and-inventory.md)、[ADR-024](024-coordinated-embedded-backend-restore.md)
- 代码证据：[`deis-control.example.toml`](../../crates/deis-control/deis-control.example.toml)、[`api/authz.rs`](../../crates/deis-control/src/api/authz.rs)、[`api/catalog/assignments.rs`](../../crates/deis-control/src/api/catalog/assignments.rs)、[`api/catalog/routing_table.rs`](../../crates/deis-control/src/api/catalog/routing_table.rs)、[`runtime_client/mod.rs`](../../crates/deis-control/src/runtime_client/mod.rs)、[`deis-runtime/dispatcher/auth.rs`](../../crates/deis-runtime/src/dispatcher/auth.rs)、[`deis-runtime/config.rs`](../../crates/deis-runtime/src/config.rs)、[`deis-proxy/catalog/client.rs`](../../crates/deis-proxy/src/catalog/client.rs)

## 背景与当前最严重风险

当前 `node_preshared_keys` 是 fleet 范围的 bearer secret。一个值同时用于：

- runtime 与 proxy 的 gRPC 注册握手；
- 节点注册、注销和续租；
- runtime 拉取 assignment 与 bundle；
- proxy 拉取整个 routing table；
- Control 调用 runtime 的部署、触发、健康与管理 RPC。

assignment 当前对所有 runtime 返回同一份环境集合，包含已解密的环境 secrets；请求路径中的 `node_id` 主要是日志与未来 placement 的占位，不是由凭证证明的授权主体。注册虽然记录 PSK 指纹并避免把轮换中的新 PSK 反向交给旧持有者，但任何持有同一个 PSK 的进程仍可声称任意空闲 `node_id`、读取全 fleet assignment、下载任意已知 bundle、读取 routing table，并以 Control 身份调用接受同一 PSK 的节点管理面。

因此当前改进只解决了“无凭证不开放”和 fleet PSK 轮换一致性，没有解决身份与权限的联结。一次 runtime、proxy、配置文件或日志事故可以横向扩大成整个 installation 的机器管理权限与应用 secret 暴露。这是 ADR-013 尚未落地部分中风险最高的一项。

## 决定

Production 模式使用统一 `MachinePrincipal`，以双向 TLS 证明每个服务实例的独立身份，并以短期、受众限定、资源限定且绑定持有密钥的 EdDSA capability 表达每次敏感操作的权限。PSK 只存在于受控迁移窗口，不能成为长期兼容路径。

身份回答“调用者是谁”，capability 回答“该调用者此刻可以对什么执行什么操作”。二者必须同时成立；网络位置、请求中的 `node_id`、hostname、HTTP header 或 capability 本身都不能替代机器身份。

## 统一 MachinePrincipal

每个 runtime、proxy、store、queue、telemetry、provider driver、topology driver 与 Control 实例各有一个持久且不可复用的 `MachinePrincipal`：

```text
MachinePrincipal {
  id: UUIDv7,
  spiffe_id: spiffe://<trust-domain>/deislet/machine/<id>,
  organization_id: UUIDv7,
  installation_id: UUIDv7,
  service_kind: control | runtime | proxy | store | queue |
                telemetry | provider-driver | topology-driver,
  state: pending | active | suspended | revoked,
  auth_epoch: uint64,
  created_at, activated_at, suspended_at, revoked_at,
  labels, attested_facts
}
```

- SPIFFE URI 是证书 URI SAN 中的唯一规范主体名；大小写、转义或 path 的另一种拼写不产生别名。
- `service_kind`、organization、installation、labels 与 placement 是 Control 中受审计的授权事实，不从调用者提交的证书扩展、hostname、地址或 header 直接相信。
- 进程重启可以继续使用同一 principal 并轮换密钥；克隆数据目录不能产生第二个有效实例。运行实例另带 UUIDv7 `instance_id`，registration lease 绑定 `(principal_id, instance_id)`。
- 一个 principal 只有一个 `service_kind`。同时运行 runtime 与 proxy 的主机使用两个 principal，避免“同一台机器”被错误等同为“同一权限主体”。
- `suspended` 暂停新 registration、assignment、capability 与管理操作；`revoked` 永久递增 `auth_epoch` 并撤销证书。ID 不删除、不复用。

## 持久模型

Control 的权威数据库至少保存以下关系；实现可以拆表，但不能降低约束：

| 记录 | 必须持久的事实 |
|---|---|
| `machine_principals` | principal、规范 SPIFFE ID、organization、installation、kind、state、auth epoch 与审计时间 |
| `machine_enrollment_tokens` | token ID、salted hash、允许的 kind/installation/labels、单次或有限使用次数、expiry、consumed/revoked 状态；不保存明文 |
| `machine_certificates` | principal、serial、issuer、SPKI SHA-256、not-before/not-after、状态与替代关系；不保存私钥 |
| `machine_registrations` | principal、instance、node ID、kind、声明与验证后的 management address、lease generation、到期时间、最后证明时间 |
| `machine_assignments` | principal 或经审计 selector、deployment plan/generation、目标角色、有效期与 revision |
| `machine_capability_issuances` | `jti`、issuer key ID、subject、audience、scope/resource 摘要、`cnf` key thumbprint、not-before/expiry、auth epoch 与状态；不保存 bearer value |
| `machine_revocations` | principal、certificate serial、capability `jti` 或 issuer key的追加式撤销事件与原因 |
| `machine_security_events` | enroll、renew、register、assign、issue、deny、suspend、revoke、legacy use、strict transition 的 actor、request/operation/correlation ID 与结果 |

规范 SPIFFE ID、certificate serial、活动 SPKI、活动 `(principal_id, instance_id)` registration 和同 installation 的活动 `node_id` 都有数据库唯一约束。状态、auth epoch、assignment revision 与 registration lease 通过 compare-and-swap 更新；不能靠进程内 map 在多 Control 实例间决定所有权。

私钥只在对应实例生成并保存在权限受限的本地 keystore、TPM/HSM 或操作系统密钥设施。Control、bundle、assignment、snapshot、backup manifest、日志和审计均不得携带节点私钥、enrollment 明文或可重放 capability。

## 独立 machine listener 与配置

Human API、machine API 和应用 data plane 使用独立 listener 或可证明等价的独立 protocol/auth boundary。Deislet 的 production 基线采用独立 listener：

```toml
[machine_listener]
bind_addr = "0.0.0.0:8443"
server_certificate = "/etc/deislet/pki/control-chain.pem"
server_private_key = "/etc/deislet/pki/control-key.pem"
client_trust_bundle = "/etc/deislet/pki/machine-roots.pem"
require_client_certificate = true

[machine_identity]
spiffe_id = "spiffe://cluster.example/deislet/machine/<uuidv7>"
certificate = "/etc/deislet/pki/machine-chain.pem"
private_key = "/etc/deislet/pki/machine-key.pem"
trust_bundle = "/etc/deislet/pki/control-roots.pem"
```

具体字段名可以随配置模型调整，但语义固定：

- machine listener 强制 TLS 1.3 或当前行业安全基线、验证 client certificate chain、有效期、URI SAN、撤销状态与本地 principal 状态；不得接受明文回落。
- human session/API token 不能访问 machine listener；machine certificate 也不能访问 human 管理 API，除非另有明确 service action capability。
- 不信任反向代理注入的 principal header。TLS 可以在受控 sidecar 终止，但 sidecar 与服务间必须使用经过认证的 Unix socket 或再次 mTLS，并把已验证身份放入不可由普通客户端到达的协议字段。
- bundle、assignment、routing、registration/renewal 和 Control-to-node management 均迁到 machine listener。公开应用 route 永不挂载这些路径。
- runtime、proxy、store、queue 与 telemetry 的 management listener 与应用 data listener 分离。ADR-023/024 的迁移和恢复管理员调用使用各自 principal 与 operation capability，不再依赖长期静态管理员 bearer token。runtime 的应用 data listener 同样只接受互证 TLS：caller 必须持有本 installation 签发的 proxy 角色证书（URI SAN `deislet/proxy/...`），否则握手即断，无任何明文回落（2026-09 落地：`deis_runtime::dispatcher::DataPlaneAcceptor`，验收见 `crates/deis-runtime/tests/management_auth_test.rs` 与 `crates/deis-proxy/tests/upstream_mtls_test.rs`）。
- store、queue 与 telemetry 持有各自的机器身份：`spiffe://<trust domain>/deislet/store/<id>`、`.../queue/<id>`、`.../telemetry/<id>`，由同一 installation CA 签发，配置在各自的 `[machine_identity]`；证书若命名其他角色则拒绝启动（2026-09 落地：`deis_identity::names`、`deis_capability::ServiceIdentityConfig`）。同一身份用于两处：
  - 三者的 gRPC listener 只提供 TLS，出示自身证书（`deis_identity::accept_tls`），不存在明文 listener，bearer capability 不会以明文出现在网络上。listener 不要求客户端证书（人类 CLI 没有机器身份），调用权限仍由每次调用携带的 capability 决定。所有调用方按「角色」校验服务端：证书必须链到 fleet trust root，且命名所要访问的服务种类（`VerifyServerIsOfKind`），同一 CA 签给 queue 的证书不能冒充 store。runtime 与 Control 用自身机器身份的 trust root 与 trust domain 校验，CLI 用 `DEIS_SERVICES_CA` 或 `[services] ca` 指定的 root。
  - capability JWKS 只挂在 Control 的 machine listener 上，human listener 不再提供；三者以 mTLS 拉取（`deis_capability::ControlClient`），出示自身证书，并要求对端证明同一 trust domain 的 Control 名称。`control_endpoint` 必须是 `https://`，非 loopback 的 `http://` 在启动时即被拒绝。
  - 尚未实现：capability 与调用方证书的 `cnf` 绑定（本节的 TLS 只防止传输中被读取或被交给错误的机器，不防止被复制的 capability 在有效期内重放）。所有 Prometheus 端口（telemetry 的 `metrics_listen`，以及 store/queue 可选的 `metrics_listen`）都是明文 HTTP，因此只允许绑定 loopback IP，其他地址启动即拒绝。
- Production 缺少独立 listener、client trust、server identity 或认证 Unix socket 时只进入 diagnostic/not-ready。Dev 可以由本地 ephemeral CA 自动配置，但仍经过相同 principal 与授权代码路径。

## Enrollment、证书续期与 Registration

### Enrollment

1. 管理员或安装自动化通过 human 管理面创建一次性、短期、scope-restricted enrollment token；响应只显示一次明文。
2. 待加入实例在本机生成 Ed25519 密钥，通过 machine enrollment endpoint 提交 CSR、token、期望 `service_kind`、installation 与可验证 attestation。
3. Control 在一个事务中消费 token、创建或激活 principal、记录 SPKI、签发短期证书并追加审计。并发重放只有一个成功；失败不得消耗成“成功但无证书”的未知状态。
4. 返回证书链、trust bundle 版本和 principal ID，不返回 CA 私钥或其他节点材料。

Enrollment token 不是机器日常凭证，不能读取 assignment、bundle、routing 或调用任何 management action。

### Renew

- 续期请求必须在 mTLS 下由当前 active principal 发起，同时以新私钥签署 CSR proof-of-possession。
- 新证书保持同一 SPIFFE ID，记录 predecessor/successor 和新的 SPKI；旧、新证书只在有界 overlap 内同时有效。
- 机器证书签发寿命不少于 72 小时（`deis_identity::MIN_CERTIFICATE_LIFETIME_SECS`）：节点在控制面失联期间最多靠已有材料运行 24 小时，其间证书无法续期，所以证书必须撑过这个窗口并留出事后续期的余量。更短的证书在加载时即被拒绝（启动与轮换都一样，轮换失败时保留正在使用的材料）；剩余不足 48 小时时每个检查周期都告警。默认在已用三分之二寿命前续期，带抖动与退避；不能靠延长 leaf 寿命替代可靠续期和撤销。
- 已过期、suspended/revoked principal、旧 auth epoch、错误 kind/installation 或未经证明的新密钥不能续期。离线过期实例必须重新 enrollment，不能用过期证书自救。

### Registration

- URL/body 中的 `node_id` 必须映射到已认证 principal；调用者不能声称另一个 principal 的 ID、role 或 address。
- Control 验证 certificate principal、持久 kind、registration assignment 和 address ownership 后写入 lease。runtime 不能注册为 proxy，proxy 不能注册 runtime management endpoint。
- registration response 只返回该 principal 的租约与 reconcile 参数。注销只能终止自己的 `(principal_id, instance_id)`；管理方移除另走 human API 并审计。
- address 变化、同 node ID 争用、克隆实例与过期 takeover 都使用持久 lease revision/CAS。探测地址只能辅助确认活性，不能证明身份。

## 授权矩阵

默认拒绝。每个 allow 都同时匹配 active MachinePrincipal、service kind、installation、当前 assignment/operation、mTLS channel 与 capability；“持有可信证书”本身没有全 fleet 权限。

| 操作 | 允许的主体 | 资源与动作约束 |
|---|---|---|
| `enroll` | 未注册实例 + enrollment token | 仅 token 固定的 installation、kind、次数与期限 |
| `renew` | principal 自身 | 同 SPIFFE ID、新 SPKI proof、当前 auth epoch |
| `register/refresh/deregister` | runtime 或 proxy 自身 | 只能操作自身 registration；kind 与 node ID 匹配 |
| `assignments.read` | runtime | 只返回该 principal 当前 placement；secrets 进一步受 deployment/version capability 限定 |
| `routing.read/observe` | proxy | 只返回所分配 installation/zone 的完整快照；observe 只能代表自身且匹配冻结 proxy target 与 generation |
| `bundle.read` | runtime | 精确 app/environment/version/digest 与未过期 assignment；不得按可猜路径下载其他 bundle |
| `deployment.prepare/activate/abort/status` | Control | 精确 target principal、plan ID、deployment generation、动作与重试 identity |
| `trigger.cron/queue` | Control 或指定调度主体 | 精确 trigger、occurrence/message、deployment generation；不能调用任意 handler |
| `health/status` | 受监控主体或 Control | 仅必要字段；详细 inspector 需独立 scope |
| `store/queue/telemetry.data` | runtime invocation | 精确 tenant、binding、operation、deployment/version 与 audience |
| `migration.fence/inventory/release` | Control migration coordinator | 精确 ProductOperation、backend incarnation、tenant、phase 与 fence identity，符合 ADR-019/023 |
| `restore/status/topology` | Control restore coordinator 或 topology driver | 精确 restore operation、installation、driver build 与阶段，符合 ADR-024 |
| `principal/cert/assignment/revoke.manage` | human 管理主体 | 不由 machine certificate 自动取得；需要相应 RBAC、step-up 与审计 |

ADR-009 的冻结 runtime/proxy target 集合由 principal ID 表达，而非可复用地址或自由文本 node ID。部署 observation 必须由同一 principal 的认证通道产生；未在 plan target 中的真实节点也不能替别人 ACK。

## mTLS 与短期 cnf-bound EdDSA Capability

Capability 采用行业标准 JOSE/JWT 表达并由 Ed25519/`EdDSA` issuer key 签名；后续可以增加语义等价的 CWT/COSE 编码，但不能改变 claim 与验证规则。至少包含：

```json
{
  "iss": "spiffe://<trust-domain>/deislet/control/<issuer-id>",
  "sub": "spiffe://<trust-domain>/deislet/machine/<principal-id>",
  "aud": "deis-queue:data",
  "jti": "<uuidv7>",
  "iat": 0,
  "nbf": 0,
  "exp": 0,
  "org": "<organization-id>",
  "installation": "<installation-id>",
  "auth_epoch": 7,
  "actions": ["queue.pull", "queue.ack"],
  "resources": [{"tenant": "...", "binding": "...", "generation": "..."}],
  "cnf": {"jkt": "<RFC-7638-thumbprint-of-holder-Ed25519-key>"}
}
```

验证端必须验证 signature/`kid`、issuer、精确 audience、subject 与 mTLS principal、`nbf/exp`、最大寿命、auth epoch、actions、全部 resource selectors、deployment generation、`cnf.jkt` 持有证明和紧急撤销状态。未知 claim 可以保留，但未知 critical version/algorithm/action 不能降级接受。算法固定 allowlist，不接受 token 自选 `none` 或对称算法。

Capability 默认 5 分钟、最长 15 分钟，不使用长期 refresh token。TLS exporter 绑定或每请求 DPoP-style Ed25519 proof 必须证明调用者持有 `cnf` 对应私钥，单独窃取 JWT 不能重放；接收端校验 method、authority、规范 path、body digest、nonce/`jti` 与短时间窗。会产生副作用的管理调用继续使用 ADR-009/019/023/024 定义的 operation idempotency 与 CAS，不能把 proof replay cache 当成业务幂等。

当前 store/queue 的 `HMAC(shared_secret, tenant)` token 只隔离 tenant，任何取得根 secret 的进程可以铸造全部 tenant token，也不能表达 binding、operation、version、audience、expiry 或持有者。它必须迁移到上述 capability；地址与 token 放进 assignment 不等于授权完成。

### 节点侧：自行申领、离线缓存

- 部署消息（`ApplyDeployment` 与 assignment 的 `env`）不携带任何 capability 或 secret 值。节点以自己的证书身份经机器通道调用 `IssueCapability`，subject 是节点自身的 SPIFFE 名字，有效期 15 分钟；某个租户的存储客户端一建好就立即申领（不等一个续签周期），剩余 5 分钟时续签（`crates/deis-runtime/src/deployment/capability_renewer.rs`）。secret 值只经 snapshot + `FetchSecretValues` 取得；没有 `environment` 声明的 manifest 不绑定任何 secret，也不会收到任何 secret。
- 节点在 `<data_dir>/offline-cache/state.bin` 保存离线缓存：最后一次完整的 assignment 列表、这些 deployment 按 revision 取得的 secret 值、以及当前持有的 capability。用节点私钥经 HKDF-SHA256 派生的密钥做 AES-256-GCM 封装，node id 作为附加数据；每次成功同步和每次续签后原子写入（0600 临时文件 → fsync → rename → fsync 目录）。只在启动时控制面不可达才读取；写入超过 24 小时的缓存整体拒绝，过期超过 24 小时的 capability 不再保留。私钥轮换后旧缓存无法打开，这是预期方向：缓存里只有节点能重新取得的东西（`crates/deis-runtime/src/deployment/offline_cache.rs`）。
- FetchSecretValues 的审计 fail-closed：「已交付」审计行写不进去时，不返回任何 secret 值；内部错误同样写审计行。

## 密钥、证书与撤销生命周期

- Platform root CA 默认离线；online intermediate、Control server cert、machine cert、capability issuer key 分开，权限与用途扩展不混用。
- trust bundle 和 capability JWKS/等价 key set 都有单调版本、not-before、overlap 与签名。消费者保留当前和明确的下一版本，拒绝未知回退。
- issuer key 轮换先发布验证 key，再开始签发，再等待最长 capability 寿命，最后退休旧 key。CA 轮换先分发双 trust，再签发新链，确认 fleet 覆盖后移除旧 trust。
- principal revoke 递增 `auth_epoch`、停止签发并发布证书/主体撤销；短期 capability 自然到期之外，紧急 denylist 支持 principal、certificate serial 与 `jti`。安全敏感服务对超过 freshness 上限的撤销视图 fail closed。
- 私钥文件最小权限、拒绝 symlink 与 group/world writable ancestor；支持 TPM/HSM key handle。轮换使用原子 replace，旧私钥在 overlap 结束并取得 durable evidence 后销毁。
- 所有时钟必须在配置容差内；默认允许的 token skew 不超过 30 秒。超界节点 not-ready，不能通过扩大 token 有效期掩盖。
- backup 保存 CA/issuer 恢复所需的加密材料与 trust metadata，不保存节点私钥。恢复演练验证签发、撤销、bundle 轮换和旧 epoch 拒绝。

### 实现现状：签发记录、吊销与 T8（2026-09）

- **签发记录**：control 签出的每个 capability（节点经 `IssueCapability`、人类经 `/storage-token`、control 自己调 queue）在交出之前写入 `capability_issuances` 与 `capability_issuance_resources`（迁移 `0005_capability_issuances.sql`：`jti`、subject、node id、audience、`kid`、auth epoch、`iat`/`exp`、每个 tenant/binding/generation）；写不进去就不签发。这是本 ADR `machine_capability_issuances` 的第一步，尚无 `cnf` 与状态列。
- **节点签发范围**：`IssueCapability` 只签节点实际运行的 tenant：audience 必须是按 tenant 校验 binding 的（store、queue、telemetry），每个 resource 的 tenant 必须来自 `deployments_visible_to(node)`，binding 必须属于该 audience，generation（若给出）必须是该 deployment 的 routing generation；telemetry 只签 `ingest`（节点运行的 tenant，外加总可申请的 `@platform`），任何 `read` 都拒绝；任何一条不满足就整体拒绝并写审计 `capability.issue`。`deployments_visible_to` 只把当前 assignment 给在 fleet 中的节点（已注册或静态配置），进行中的 plan 按 `target_ids`。
- **吊销**：owner API 按 `jti`、按节点、按 tenant 吊销（从签发表找出仍可能被接受的 `jti`，写入 `capability_revocations`，带其 `exp`），或递增 `auth_epoch` 做批量吊销；四个入口都是 `Verb::Admin`、全部写审计。
- **吊销不殃及旁人**：节点按 tenant 逐个申请 store/queue capability，每个 token 只含一个 tenant，按 tenant 吊销只收回点名该 tenant 的 token。store、queue、telemetry 以 `Unauthenticated`/`PermissionDenied` 拒绝节点出示的 capability 时，节点立即重新申请（距上次签发至少 30 秒，防止无法靠续期修复的拒绝变成请求风暴），不再等到剩 5 分钟才换。节点的 telemetry capability 仍是覆盖其全部 tenant 的一个 token，吊销任一 tenant 会让该节点的推送短暂被拒，批次留在队列里，新 token 数秒内到达。
- **吊销名单**：只在 machine listener 上提供（`/capability/revocation-list`），内容是未过期（含 120 秒余量，覆盖 T8 的 `exp ≥ last_sync − skew` 边界）的吊销 `jti` 与当前 `auth_epoch`，**用签发密钥签名**（与 capability 同一 `v1.<payload>.<sig>` 格式，payload 带 `typ = "deis-revocation-list"`，二者互不可冒充）。store、queue、telemetry 用各自的 `[machine_identity]` 经 mTLS 从 machine channel 拉取（与拉 JWKS 同一个 `ControlClient`、同一个 `control_endpoint`，http 只允许 loopback），每 30 秒一次；校验方用同一 JWKS 验签，拒绝早于两分钟、来自未来或早于上一份的名单（防重放），epoch 只升不降。store 的 kv/sql/blob 共用一个校验器，同一份名单三者一起生效。
- **T8 节点判定**：宽限只给 subject 为节点 SPIFFE 名（`spiffe://<trust domain>/deislet/node/<id>`，由 `deis-identity` 解析）的 capability；校验方可限定 trust domain。
- **重启语义**：校验方在 control 不可达时重启，没有 last sync，也没有公钥（JWKS 不落盘），因此拒绝一切 capability，指标报告 degraded。不持久化 last sync：单独持久化它会让宽限期接受在那次同步里已被吊销的 capability。
- **指标**：`deis_auth_degraded`、`deis_auth_degraded_since_seconds`、`deis_auth_grace_accepted_total`、`deis_auth_revocation_last_sync_seconds`，label `service`。

## Dual 到 Strict 的滚动顺序

迁移可用性不能被描述为已经实现安全隔离。顺序固定为：

1. **Inventory**：列出全部 PSK 使用路径、机器实例、角色、地址、assignment、管理调用与配置来源；持久建立 principal/certificate/registration/capability 模型和审计，但不改变流量。
2. **Dual trust bootstrap**：启用独立 machine listener、内部 CA、issuer 与 trust bundle；逐实例 enrollment。实例先能 mTLS 健康/续期，再承接业务。PSK 调用逐次记录 endpoint、peer、claimed ID 和时间，并令 production readiness 降级。
3. **Read path enforcement**：按 `routing → bundle → assignments` 切换到 principal 授权。每一路只有在所有目标实例已用 mTLS 成功、负面测试通过且旧路径无调用超过至少两个 registration TTL 后，才关闭该 endpoint 的 PSK；一旦关闭不得按请求自动 downgrade。
4. **Management enforcement**：Control-to-runtime/proxy/store/queue/telemetry 和 migration/restore 调用改为 mTLS + operation capability。先验证 PREPARE/ACTIVATE/ABORT、trigger、fence、inventory、release 与 status 的精确权限，再逐 endpoint 关闭 PSK/static admin token。
5. **Data capability enforcement**：runtime 到 store/queue/telemetry 改为 cnf-bound 短期 capability；验证跨 tenant/binding/action/generation 与重放均拒绝后，移除 shared HMAC secret。telemetry 一侧已于 2026-09 落地（`cnf` 仍待 RB-2）：deis-telemetry 经 JWKS 校验 `aud=telemetry` 的 capability，`ingest` 与 `read` 两个 binding 互不蕴含，读取按租户过滤，保留租户 `@platform` 表示节点自身记录（写）或全平台（读）；节点经 `IssueCapability` 申领，人经 `telemetry-token` 路由申领并记审计；共享 secret 与不鉴权模式均已删除（`crates/deis-telemetry/src/auth.rs`）。
6. **Strict**：配置、数据库、示例和启动参数不再接受 fleet PSK；删除 legacy listener/route 与代码，撤销并销毁旧值，保留不含 secret 的迁移审计。所有实例更新 trust 后才移除旧 CA/issuer key。

Dual 阶段的 PSK 无法区分 principal，因此仍具有旧的横向权限，不能报告 `production-secure`。不能以“某个 claimed node 已迁移”为依据拒绝来自同一共享 PSK 的降级，因为 PSK 本身证明不了是谁；关闭必须按 endpoint/fleet readiness gate 原子进行。中途失败保持当前明确阶段，重试相同 operation，不反向打开已严格关闭的入口。

## Fail-closed Readiness

Production readiness 至少检查并返回结构化 reason：

- machine listener 已启用，TLS/client trust/server identity 有效，公开 data/human listener 未挂载 machine route；
- 不再配置、接受或发出 fleet PSK、shared storage HMAC 或静态 backend admin bearer credential；
- 每个必须服务的实例都有 active principal、未过期证书、当前 registration lease 和匹配 role/assignment；
- CA、trust bundle、capability issuer、JWKS 与撤销视图未过期且可轮换，下一次轮换不会早于恢复余量；
- issuer 能签发，验证端能验证并拒绝错误 audience/action/resource/`cnf`；
- principal/auth epoch、assignment revision、部署冻结 target 与 observed registration 无冲突；
- audit sink 可写，clock skew 在 token、certificate 和 lease 容差内；
- strict migration gate 完成，legacy 最近使用计数为零且代码路径已禁用。

任一项失败时服务可以提供不含敏感信息的 liveness 和本地诊断，但不能返回 ready、签发新 capability、接收新的生产 assignment 或执行高权限管理变更。已有 data plane 是否继续短时服务由明确的故障策略决定；不得把认证故障静默变成匿名或 PSK 回退。

## Acceptance Matrix

| 场景 | 必须结果 | 最小证据 |
|---|---|---|
| 合法一次性 enrollment | 创建一个正确 kind 的 principal 与证书 | 数据库约束、证书 SAN、审计与端到端测试 |
| enrollment 并发重放/过期/错 scope | 全部拒绝且不产生第二主体或孤儿证书 | 并发测试、事务后状态 |
| CSR 不持有新私钥 | 拒绝 | proof-of-possession 负面测试 |
| 合法 renew 与 overlap | 同 SPIFFE ID，新 SPKI 生效，旧证书按期退出 | 时间可控集成测试、cert lineage |
| revoked/suspended/expired principal renew | 拒绝且不签发 | CA/Control 端到端测试 |
| runtime 注册自身 | 持久 lease 绑定 principal/instance/node/kind | API + 数据库观察 |
| runtime 冒充另一 node 或 proxy | 拒绝，原 registration 不变 | spoof/role mismatch 测试 |
| 两个实例克隆同一 identity | 最多一个 lease owner；争用可见且不静默覆盖 | 并发/CAS/expiry takeover 测试 |
| runtime 读取 assignments | 只得到自身 placement 与获准 secrets | 两 runtime 差异化 fixture |
| 未分配 runtime 或 proxy 读取 assignments | 拒绝，不返回空集伪装成功 | HTTP/gRPC 负面测试 |
| proxy 读取/ACK routing | 仅本 zone；只能 ACK 自身冻结 target/generation | ADR-009 跨服务测试 |
| runtime 下载 bundle | 仅精确获配 version/digest | 猜测 ID、旧 generation、跨 app 负面测试 |
| Control PREPARE/ACTIVATE/ABORT | 只作用于 capability 固定 target/plan/generation/action | 丢响应、重放、动作替换、错误 target 测试 |
| runtime 或 proxy 调管理 RPC | 即使证书可信也拒绝 | role escalation 测试 |
| 合法 store/queue capability | 仅受众、tenant、binding、action、generation 内成功 | 真实服务 E2E |
| capability 跨 tenant/binding/action/audience | 全部拒绝且不改变数据 | 每维独立负面测试 |
| capability JWT 被盗但无 `cnf` 私钥 | 拒绝 | PoP 缺失/错误 key/replay 测试 |
| capability 过期、未来 `nbf`、旧 auth epoch、revoked `jti` | 全部拒绝 | 可控时钟与撤销传播测试 |
| issuer/CA key 轮换 | overlap 内不中断，退休后旧链/key 拒绝 | 多进程滚动与重启测试 |
| 撤销视图过旧或 issuer 不可用 | 敏感操作 fail closed，readiness false | 故障注入测试 |
| Control 多实例竞争 assignment/registration | CAS 只产生一个权威 revision | 双 Control 并发测试 |
| Dual 节点仍使用 PSK | 可用但 readiness 明确 degraded，调用有审计 | upgrade fixture 与指标 |
| 已关闭 endpoint 收到 PSK | 拒绝且绝不自动 downgrade | strict regression 测试 |
| strict 启动残留 PSK/shared secret | not-ready 或配置拒绝，不输出 secret | 配置矩阵与日志扫描 |
| secret/cert/token/proof 出现在日志、API、crash 或 snapshot | 测试失败 | redaction/capture/backup 检查 |
| migration fence 与 restore topology 操作 | 只接受精确 coordinator principal、operation 与 incarnation | ADR-023/024 contract E2E |
| 部署冻结 target 被撤销或 registration 过期 | plan 不宣称 active，重对账或安全失败 | ADR-009 reconcile/restart 测试 |

测试必须跨真实 listener 和真实 TLS stack，不能只单测 claim parser。Linux production、macOS/Windows 开发节点至少各完成 enrollment、renew、registration、assignment 和 bundle 流程；provider driver 使用同一主体/能力语义，但由其目标平台的安全传输实现。

## 可执行切片与完成条件

### M1：身份基础与机器控制通道

完成条件全部满足才可关闭该切片：

- 持久 principal/enrollment/certificate/registration/revocation/audit 模型和迁移已落地，多 Control CAS 测试通过；
- Control、runtime、proxy 有独立 machine listener/client 配置、真实 mTLS、SPIFFE URI 验证、enroll/renew 与原子 key persistence；
- registration、routing、assignment、bundle、runtime management 全部按上述矩阵鉴权；assignment 至少支持按 principal placement 返回，不再把 fleet 明文 secrets 交给任一 PSK 持有者；
- ADR-009 的 runtime/proxy target 与 observation 改用 principal ID，重启、lease expiry、revoke 和错误 ACK 测试通过；
- dual migration 有指标、审计、状态与不可逆 endpoint gate；strict 配置拒绝 PSK，production readiness fail closed；
- Linux 真实进程 E2E、macOS/Windows 开发客户端流程、证书轮换/撤销/时钟/Control HA 故障注入和日志 secret 扫描通过。

### M2：持有证明能力与内部服务管理

完成条件全部满足才可关闭该切片：

- capability issuer、public key distribution、`cnf` proof、auth epoch、紧急撤销与轮换已落地；
- runtime 到 store/queue/telemetry 使用精确 tenant/binding/action/generation capability，shared HMAC secret 已从 strict 模式与代码路径移除；
- Control 到 runtime/proxy 以及 ADR-023/024 的 backend/topology management 使用 operation-scoped capability，静态 admin bearer token 已从 strict 模式移除；
- Acceptance Matrix 中跨资源、重放、旧 generation、过期、撤销、issuer/CA rotation、断网与重启测试通过；
- conformance contract、registry evidence 和运维手册能从签发记录、服务 observation 与审计事件重建一次授权决定。

ADR-025 只有 M1、M2、strict fleet 迁移与生产演练全部完成才算实现完成。新增表、能签证书或单个 happy-path E2E 都不是完成证据。

## 非目标

- 不用机器身份替代 human account、session、API token、RBAC、MFA 或 provider 用户授权；机器不能仅凭证书创建应用、变更权限或管理其他 principal。
- 不把 SPIFFE/SPIRE 某个具体发行版规定为唯一实现。SPIFFE URI 语义与可验证工作负载身份是标准；Deislet 可内建 CA，也可接入符合契约的外部工作负载身份系统。
- 不声称 mTLS 能解决恶意应用代码、runtime sandbox escape、供应链签名、SSRF/egress policy 或宿主机 root compromise；这些有独立边界。
- 不在本 ADR 改变 ADR-009 的发布状态机、ADR-019/023 的迁移语义或 ADR-024 的恢复线性化点；这里只把其中的 actor、target 与权限从共享 secret 收紧为可验证主体和 operation capability。
- 不承诺跨地域全球 PKI 服务。本项目仍是 mini 平台，但单机 embedded 与多节点/HA 后端必须遵守同一身份、授权、轮换和 fail-closed 契约。
- 不为 legacy PSK 提供永久兼容、按节点伪造的衍生 token 或“仅在私网”例外。无法证明 principal 的凭证不能进入统一抽象的严格实现。

## 后果

- fleet PSK、shared storage HMAC 与长期 backend admin token 都成为有终点的迁移机制，不再是 production 安全模型。
- assignment 从“所有 runtime 相同”升级为真实 placement/授权接口；部署、路由与触发的 target/ACK 需要稳定 principal ID。
- CA、issuer、证书续期、撤销分发、时钟和 audit 成为 production readiness 的硬依赖，运维复杂度增加，但单实例泄漏不再天然取得全 fleet 权限。
- Dev 仍可一键启动 ephemeral identity domain，但不维护第二套匿名内部协议；这使开发路径持续覆盖生产鉴权代码。
- Edge Canon 应把主体、capability claims、授权矩阵与 conformance cases 固化为 provider-neutral 契约。Cloudflare、EdgeOne 或其他后端可以映射到平台自有 service identity/token，但必须报告哪些保证由原生设施实现、哪些由 driver/shim 实现，不能用“供应商已认证”替代逐项证明。
