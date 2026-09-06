# ADR-006：统一部署契约与 Provider Deployment Drivers

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-005
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

当前 `deis build` 可以生成 Cloudflare、Deno、Tencent 和 Deislet 目标，但外部目标主要停留在 artifact 转换；`deis deploy` 只负责自建 Deislet 后端。资源 ID、环境、secret、trigger、preview、激活和回滚仍需用户进入各 provider 工具处理。

统一标准要求最终部署后的应用仍满足同一语义。只统一源码和 build artifact、却把资源绑定与发布交给不受控外部步骤，会在最后一公里重新产生 provider 分支和配置漂移。

产品负责人决定选择统一部署契约，同时允许实现内部直接调用各 provider 官方 CLI。

## 决定

Deislet 对用户、CI 和控制台提供统一部署接口。每个合规后端由 provider driver 实现该接口；driver 内部可以调用固定并验证过的官方 CLI、官方 API/SDK，或组合使用两者。

CLI/API 是 transport 和 provider automation 细节，不构成应用标准，也不改变统一 deployment domain model。

## 统一部署领域模型

核心对象：

- `ProviderInstallation`：provider 账户/组织、区域和认证引用。
- `Target`：通过某 Edge Canon 版本认证的 provider 产品与配置组合。
- `Application`：标准应用身份。
- `Environment`：production、preview 或用户命名环境。
- `ArtifactVersion`：不可变 Edge Canon build output 与 digest。
- `BindingPlan`：逻辑资源到 provider 资源的解析结果。
- `ProviderVersion`：provider 返回的不可变版本 ID。
- `Deployment`：目标环境当前激活的一个或多个版本及流量策略。
- `Operation`：可恢复、可审计的部署操作和步骤状态。

Provider 原始 ID 和 URL 作为 metadata 保存，但应用不依赖其格式。

## 统一生命周期

```text
validate
   ↓
preflight credentials/account/region/quotas
   ↓
plan artifact + bindings + triggers + routes + limits
   ↓
prepare/provision resources
   ↓
upload immutable version
   ↓
verify bindings + preview + Edge Canon smoke
   ↓
activate/promote
   ↓
observe/reconcile
   ↘ rollback
```

所有 driver 都必须实现完整生命周期；不能只实现 upload 便宣称 deploy 支持。

## Provider driver 契约

建议的内部接口：

```text
inspect_installation
preflight
plan
prepare_resources
upload_version
configure_routes_and_triggers
create_preview
verify_version
activate
deployment_status
reconcile
rollback
delete_preview
```

每个方法必须：

- 接受 operation ID、desired-state digest 和显式 target。
- 返回结构化、provider-neutral result/error。
- 保存 provider request/deployment/version ID。
- 可在进程中断后查询并继续。
- 对相同 desired state 幂等。
- 明确是否产生外部副作用以及是否可回滚。

## 使用官方 CLI 的规则

允许调用 Wrangler、`edgeone makers`、`deno deploy` 等官方 CLI，但必须满足：

1. **版本固定**：driver manifest 声明经过测试的精确 CLI 版本或允许范围。
2. **来源验证**：从受信 registry/发行源安装，校验 lock/digest；不能任意调用 PATH 中未知版本。
3. **直接执行**：使用 executable + argument array，不经过 shell 字符串拼接。
4. **非交互模式**：CI/control 操作不能等待终端 prompt。
5. **结构化输出**：优先 JSON；不得长期依赖面向人的文字、颜色或表格解析。
6. **受控环境**：显式 cwd、最小环境变量、超时、process group cancellation 和输出大小限制。
7. **敏感信息隔离**：token 通过环境、stdin、0600 临时文件或 keyring 注入，不能进入 argv、artifact、日志或错误回显。
8. **输出脱敏**：stdout/stderr 在持久化前按 provider credential 和 secret 规则过滤。
9. **升级门禁**：CLI 升级必须运行 driver contract tests 和 provider live deployment conformance。
10. **失败接管**：CLI 退出/超时后，driver 先通过 provider status 查询实际状态，再决定继续或补偿，不能盲目重试。

若官方 CLI 缺少机器输出、资源操作或幂等查询，driver 应使用官方 API/SDK 补齐。不得为了“纯 CLI”降低统一契约。

## 凭证模型

### 本地开发

- 可以复用官方 CLI keyring/profile。
- `deis` 只请求当前 operation 需要的权限。
- 提示将访问的 provider、account、environment 和副作用计划。

### CI 与控制面

- 使用 provider service credential，不复用个人登录会话。
- 凭证加密存储，只在 driver 子进程生命周期内解封。
- 不同 provider installation、environment 和组织分别授权。
- 支持轮换、撤销、最后使用时间和审计。
- driver 子进程不能继承无关 secret 或宿主完整环境。

## Artifact 与资源绑定

- Edge Canon artifact 在上传前已经不可变并有 digest。
- provider config 是由 artifact、environment binding plan 和 target adapter 确定性生成的派生产物。
- 不允许用户在生成后手工填写 KV/D1/Blob ID 再部署。
- 资源创建/绑定结果保存到 deployment state，并参与 verify。
- secret value 不进入 artifact digest；secret 名称和版本引用进入 desired state。
- trigger、route、compatibility setting、limit 也属于 provider version/deployment 的验证范围。

## 幂等、中断恢复和 Reconciliation

每次 operation 持久化：

- operation ID。
- desired state digest。
- 当前步骤和每步输入摘要。
- provider request/resource/version/deployment IDs。
- 完成、失败和未知状态。
- 可重试分类和下一动作。

进程重启后：

1. 查询 provider 的真实状态。
2. 将外部状态与 desired state 对比。
3. 已完成步骤不重复创建资源。
4. 状态未知时先进入 reconcile，不直接 activate。
5. activate 后本地记录失败时，可以通过 provider deployment ID 恢复正确状态。

## 统一状态与错误

至少统一：

- `planned`
- `preparing`
- `uploaded`
- `verifying`
- `ready`
- `activating`
- `active`
- `degraded`
- `failed`
- `rolled_back`

Provider 错误映射到认证、权限、配额、配置、构建、上传、资源、验证、激活、暂时性服务和未知状态等类别。原始诊断可以脱敏后附带，但不得成为自动化唯一判断依据。

## 用户接口

统一 CLI/API 至少提供：

```text
deis target login/link/doctor
deis deploy --target <target> --environment <env>
deis preview --target <target>
deis deployments status
deis deployments promote
deis deployments rollback
deis deployments reconcile
```

命令命名最终与现有 CLI 兼容性一起设计。CI 和 Web 控制台调用同一 application service，不重复实现 provider 逻辑。

`plan` 默认展示：

- target/account/region。
- artifact digest 和标准版本。
- 将创建/变更的 resources、routes、triggers、secrets references。
- preview/production 影响。
- provider 费用或套餐风险（能获取时）。
- 是否支持回滚以及存储不随版本回滚的提示。

## Provider 初始映射

### Cloudflare

- Wrangler 负责配置验证、resource commands、version upload、deployment/status 等适合的操作。
- 官方 API 补充稳定查询、资源 reconciliation 或 CLI 不具备的结构化操作。
- 上传 version 与 activate deployment 必须分离，不能默认用一次 `wrangler deploy` 跳过统一 verify 阶段。

### EdgeOne Makers

- `edgeone makers deploy` 可用于 production/preview 上传。
- token 支持 CI/non-interactive。
- 对 version、rollback、resource binding、status 缺失的部分使用官方 API或在认证前标为未完成。

### Deno Deploy

- `deno deploy` 可处理 app、env、database、deployment 和 logs。
- driver 仍需固定 context，禁止 CLI 隐式当前 organization/application 造成误部署。

### Deislet

- 现有 control API 迁移为相同 driver/lifecycle 契约。
- 自建目标不能保留一套特殊的用户侧部署语义；它只是一个内置 provider driver。

## 测试

1. Provider-neutral driver contract tests。
2. Fake provider 故障注入：超时、重复响应、进程退出、状态未知、部分资源成功。
3. CLI parser fixtures，但仅针对固定版本和结构化输出。
4. 每个 provider 的 sandbox account live tests。
5. Preview → smoke → activate → status → rollback 全流程。
6. Credential redaction、argv/process environment 和日志泄漏测试。
7. CLI/API 中断后的 reconciliation 测试。
8. Edge Canon conformance app 的真实部署测试。

## 已落地的凭据执行边界

Control 的凭据 binding 不再是未解析引用：它必须先在同一 organization/backend 的加密 vault 中建立，随后才可
注册 provider installation。每次密文轮换产生不可变 revision，AES-256-GCM associated data 固定 organization、
backend、binding 与 revision；吊销会同步禁用 installation 和未结束租约。

provider operation 取得 durable owner/epoch 后，orchestrator 才构造不可序列化的 execution context。driver 用该
context 从 broker 申请短租约；数据库在同一授权判断中复核 operation、target、installation、binding、当前
credential revision 和两个 lease expiry。凭据 material 不属于可序列化 request/receipt/error，Debug 固定脱敏并
在 drop 时清零。provision、rotation、lease、release、expiry、revocation 和 denial 只记录无敏感值的 append-only
audit metadata。

Control 的持久 reconciler 定期收敛过期租约并记录审计。这个基础实现仍不表示公共管理面或真实 provider 已完成；
API/CLI/Web 接线、受信 driver 安装、各 driver 实际使用、KMS/envelope key、机器 mTLS/capability 与 live security
evidence 仍是后续验收项。

## 跨平台发行身份与安装树

`driver.distributionDigest` 统一定义为 RFC 8785 canonical `provider-driver-release` 文档的 SHA-256，而不是某一个
操作系统 archive 的摘要。同一个逻辑 driver ID/version 只有一个 release identity；release index 再为
`linux-amd64`、`linux-arm64`、`macos-arm64`、`windows-amd64` 分别固定 archive、完整文件表、tree digest、driver
component 和官方工具 component。这样各平台可以有适当的二进制/依赖实现，但 target pin 与 lifecycle 契约仍只有
一个，不会因主机差异产生第二套应用抽象。

官方工具的 manifest digest 继续表示经评审的上游 source distribution；release index 的 component file set 和
materialization digest 负责证明该来源在特定主机上展开出的完整依赖树。每个 executable 必须属于该 component 的
规范有序文件集，同时也是完整 distribution file table 的 executable 成员。

安装器只接受绝对、真实、受服务身份控制的 installation root，并在进程间文件锁内工作。它从同一个已打开的 regular
archive handle 复核长度和摘要，在隔离 staging 中拒绝 link、非 regular、额外、重复、缺失、大小或摘要不符的 entry；
路径规则同时排除 traversal、反斜杠/盘符、大小写碰撞、file-as-parent 和 Windows reserved name。全部文件验证并同步后
才按 release digest + host 原子发布只读目录，随后再次遍历完整树；已有目录必须逐文件一致，绝不原位修补。崩溃留下的
专用 staging 只在持有同一安装锁时清理。

Windows 的 read-only attribute 不属于权限边界。installation root、trust root 与 rotation envelope 必须由当前 Control
进程令牌用户 SID 持有，DACL 必须以 `SE_DACL_PROTECTED` 关闭父目录继承；只有该 SID、LocalSystem 与 built-in
Administrators 可以获得写入、删除、改 DACL 或改 owner 权限。标准 allow ACE 会核对 SID；带危险权限的 object、callback
或 compound allow ACE 因有效语义更复杂而直接失败关闭。校验始终读取已打开对象 handle 的 owner/DACL，而不是先按路径
查询再相信结果；installation root 还在 installer 生命周期内保留一个不共享 delete 的目录 handle，阻止重命名或替换根。
测试 ACL 构造器只在显式 `test-support` feature 下导出，正常 Control 依赖不会获得修改 ACL 的生产 API。

Control 现把 release/host 的验证结果写入不可改写、不可删除的持久 registry，并让 active target 精确绑定该 release。
没有 pin 的旧 target 在升级时 disabled；全树校验失败会把 release 永久 quarantined 并追加审计。唯一公开执行入口只
接受由 Control 创建的 dispatcher，不再接受调用方提供的任意内存 driver。dispatcher 从 target 的 host-specific 安装树
选择唯一 driver entrypoint，在 durable claim 后再次核对全树和 executable digest，再以统一
`edge-canon-provider-driver-v1` 进程协议传入 request/outcome；owner/epoch 绑定的短租约 credential 只进入清空后的短期
process environment。公开 schema 禁止 vendor-specific envelope，进程超时或结果不明时仍按副作用规则 reconcile。

## 发行授权与信任根

archive/tree digest 只能证明内容一致，不能证明发布者授权。发行授权因此是独立于 immutable release index 的 sidecar：
`provider-driver-release-authorization` payload 以 RFC 8785 canonical JSON 精确绑定 release SHA-256、manifest SHA-256、
backend、driver ID/version 和有限有效期，再装入标准 DSSE v1 envelope；Ed25519 签名覆盖 DSSE PAE，因此 payload type
也属于签名语义。一个 release 可以拥有多个不可变 authorization，以便在不改变相同发行 bytes 的情况下续期。

Control 的 out-of-band trust root 固定 root version/expiry、key ID、公钥、有效期/撤销、角色 key set、阈值和授权最长
寿命。key ID 是 32-byte raw public key 的 SHA-256。信任根文件必须为绝对路径、regular non-symlink、小于 1 MiB；
Unix 还要求由服务身份持有且 group/other 不可写，Windows 执行上述 owner/protected-DACL 规则。Control 把每个见过的 canonical root/version 持久保存，拒绝版本
回退、跳号、同版本不同内容，以及更高版本重新激活历史上已经撤销的 key；安装根与信任根必须同时配置，否则外部执行
保持关闭。

安装开始前先验证 subject、时间和多签阈值，失败时不能产生安装目录。授权记录不可改写或删除；active target 必须同时
找到已验证 release 与至少一份授权。每次选择 executable 都用当前信任根重新验证全部候选 authorization；当前无有效
授权时立即阻断执行，但不把内容寻址的 release 误判为损坏；补充新的有效 authorization 后可恢复。只有完整安装树校验
失败才会把 release 永久 quarantine 并记录审计。这样 key revocation、root expiry 或 authorization expiry 会在下一次
执行前生效，不依赖重启时的一次性检查。

精确提交 `b185984da354e59a199fe600726e454a9c4f3ef9` 的同一 archive 已在 Linux、macOS、Windows 通过
registry/dispatcher 平台适用测试、legacy migration 和五个部署控制 crate 的严格 Clippy，并登记受 Schema 与治理脚本
约束的 evidence；它还保留前一提交在 Windows 暴露的 async `Send` 编译失败及其修复关系。该结果只证明受验证安装树到
进程边界的三平台可移植性，并不证明任何真实 provider deployment 或 conformance。

精确提交 `b574d58f1894d7a23e4d146bbb8a082e1d17c0c8` 进一步以同一 archive 在 Linux/macOS 各通过 provider
driver 52 项、Windows 43 项，三端均通过 Control provider 专项 18 项、legacy migration 1 项和严格 Clippy；共享
DSSE fixture 的真实 Ed25519 阈值签名也由 Rust verifier 验证。证据登记完整 commit/tree/archive 身份、逐主机计数及
Windows reparse/share-write 文件边界。macOS 未安装 Python `jsonschema`，因此共享 14 份 Schema/37 组 fixture 只在
Linux 执行；该环境缺项不冒充三端契约运行。

信任根演化采用 TUF root update 的核心不变量，而不是由当前根单方面替换自身。根文档新增独立 `root` 角色；轮换
sidecar 的 payload 是恰好 N+1 的 RFC 8785 canonical 新根，DSSE PAE 必须分别达到旧根 `root` 角色和新根 `root`
角色阈值。验证轮换时不以旧根文档过期为恢复阻断，但参与签名的旧/新 key 在观察时刻都必须 active 且处于自身有效期，
最终新根必须未过期。首次带外引导不能携带无从验证的轮换 envelope；数据库已有同一 canonical 最高版本时可直接重启，
只有前进到下一版本才要求 envelope。版本回退、跳号、同版本异内容、已撤销 key 重新激活，以及已经退出 key/root/release
角色的历史 key 被重新引入都失败关闭。

Control 通过与根文件相同的安全文件读取边界加载可选 rotation envelope；根与轮换在同一 SQLite 事务中持久化。
`provider_driver_trust_root_rotations` 保存前后版本、canonical envelope 摘要和双方实际签名者，记录不可修改、不可删除。
为避免上一已发布切片的 pre-root-role v1 在升级时失效，兼容解析只对既有历史保留其原 canonical identity，并把旧
release role 阈值作为旧 root role；它不能用于全新 bootstrap，下一次双阈值轮换写入显式 root role 后即退出兼容路径。
共享契约新增 rotation envelope schema、canonical N+1 正例与 payload type/signature/role 阈值反例；Rust fixture 测试会
执行真实 Ed25519 双阈值验证，而不把 Python 结构检查冒充密码学证据。该设计遵循 TUF 对连续 intermediate root、旧/新
root threshold 及只在更新结束检查最终 trusted root expiry 的要求。

精确提交 `a934ff5003dc66745db2b0a08cdbd9050225700a` 的同一 archive 已在 Linux、macOS、Windows 通过
provider driver 平台适用测试、Control provider 专项、legacy migration 与五个部署控制 crate 的严格 Clippy；Linux
另通过 15 份共享 Schema/42 组 fixture。证据保留父提交 `5f79e47` 在 Linux 首次进程启动失败以及 Windows 取证脚本
误判 Cargo stderr 的事实，不能用后续绿灯改写。runner 只在操作系统明确报告进程尚未创建的 `WouldBlock`、
`Interrupted` 或 Unix `ETXTBSY` 上做有界启动重试，仍受原调用 deadline 约束；其他错误立即失败，因而不会以重试
重复 provider side effect。该证据同时覆盖显式 root role、旧/新根双阈值、过期旧根恢复、最终新根有效期、不可变轮换
记录，以及 pre-root-role v1 历史根重启和一次性迁移。

## 发行透明度与离线证明

阈值 DSSE 只能证明受信 release signer 授权了某个发行，不能证明授权已进入可审计、可交叉见证的公共历史。统一契约
因此新增 `provider-driver-transparency-proof` sidecar；它不复制任何单一 vendor 的 REST response，而是组合行业已有的
可验证原语：RFC 6962 Merkle inclusion、C2SP signed note/checkpoint 和 timestamped witness cosignature。proof 绑定 exact
canonical authorization envelope 的 SHA-256；日志 leaf 固定为 `SHA-256(0x00 || canonical envelope)`，内部节点固定为
`SHA-256(0x01 || left || right)`。每个 entry 保存 log origin/index、inclusion path 和完整签名 checkpoint，因而运行时
不需要信任在线日志返回的 `integratedTime`，也不需要联网查询才能复核历史证明。

信任根的 transparency policy 固定日志与见证者的 note name/origin、operator、Ed25519 key ID，以及不同日志 operator
和不同见证者 operator 的阈值。日志、见证者各自不得复用密钥，且这些密钥不得与 root/release signer 角色复用；日志
operator 也不得自任见证者 operator。阈值只按不同 operator 计数，不能用同一运营主体的多次部署或多把 key 扩大票数。
log operator 阈值不得超过 v1 proof 的 16-entry 上限，避免接受永远无法满足的信任根。每个 checkpoint 必须单独达到
witness operator 阈值，整份 proof 还必须达到 log operator 阈值。log origin 与 witness name 在一个策略内分别唯一，
不能用同一 note identity 表达多个运营主体。见证时间必须位于
authorization 有效期、对应 witness key 有效期和当前时间之前；log key 至少在其中一个受信见证时间有效。保留同一日志
或见证者 key 的根轮换不能改变其 origin/name 或 operator；需要变更运营身份时必须使用新 key，避免只改显示文本重算
信任票数。已经出现在 root、release、log 或 witness 角色历史中的 key 也不能在后续轮换时改任另一角色；角色转换必须
生成新 key，旧 key 只保留在不可变历史中。

C2SP signed-note key ID 按 `SHA-256(name || "\n" || signature-type || raw-public-key)` 的前四字节计算；日志签名类型固定
为 Ed25519 `0x01`，见证签名固定为 timestamped Ed25519 `0x04`。解析拒绝未知 checkpoint extension、非 canonical
decimal/base64、控制字符、重复受信签名、错误宽度和过长输入；不认识的 note signature 可以保留以支持独立生态演化，
但任何匹配已受信 name/key ID 的损坏签名都会失败关闭。当前 v1 wrapper 有意不宣称与 Sigstore bundle v0.3 字节兼容：
Sigstore bundle 是可参考的交换格式，但其 DSSE message signature 约束与本产品已有的多签阈值 authorization 不相同；
Deislet 的统一应用/部署标准不能因此退化为某一个供应商 envelope。

Control 在 archive 解包前依次验证 DSSE 与透明 proof；两者和安装/registry 事实在同一事务中持久化。原始 proof、摘要、
authorization 摘要、log origins/operators、witness operators 和最早/最晚见证时间均不可修改、不可删除。active target
必须具有同一 host authorization 对应的 transparency row；升级时只有签名、没有 proof 的旧 active target 会自动
disabled，后续 activation 仍被独立新触发器拒绝。每次 executable selection 都从持久 canonical JSON 重新执行签名、
inclusion、checkpoint、见证阈值及时间验证，并复核派生事实，不能只相信注册时的布尔结果。

为兼容已部署的显式 root、但不静默降低新基线，旧 root 若缺 `providerDriverTransparency` 只能按原 canonical bytes
作为既有最高版本重启，以便提交受旧/新 root 双阈值保护的下一版轮换；它不能用于全新 bootstrap，也不能启用 dispatcher。
轮换到带透明策略的新根后立即退出恢复模式。当前契约 fixture 使用确定性合成日志/见证 key 和单叶树。精确提交
`05cbbf98e719c084c8c6ea8085b2038396193dba` 的同一 archive 已在 Linux/macOS/Windows 通过平台适用的 verifier、
Control provider、两项 target migration 和严格 Clippy，并登记受治理 evidence；Linux 另通过 16 份 Schema/48 组
fixture。该结果仍不等于接入真实公共日志或独立见证服务。当前 v1 只固定 C2SP Ed25519 `0x01`/`0x04` 分支；上游
ML-DSA-44 能力必须通过后续版本化统一契约引入，不能静默改变 v1 语义。

精确提交 `2f215a8777549a570a65328df3e365db22badec5` 的同一 archive 已在 Linux、macOS 与 Windows 通过
provider-driver、Control provider、两项 target migration 和五 crate 严格 Clippy；Windows 原生 54 项包括 owner、
protected DACL、非可信 writer、trust file 与 root handle pinning。正式 evidence 明确保留测试使用提升的交互式 token，
尚未把 Control 安装为使用虚拟、托管或域账户的 Windows Service，不能把 API 语义验证夸大成服务部署验收。

## 后台部署协调器

Control 启动时只在透明信任根、已验证 release registry 和 dispatcher 都成功构造后启动 provider deployment
协调器；未达到这一前提时保持外部执行关闭，而不是退回未验证入口。协调器按配置的轮询周期扫描持久 workflow，限制并发，
并为每次执行生成不同的 UUIDv7 owner。真正的高可用互斥仍由数据库内 owner/epoch lease 保证：live lease 不被接管，过期
lease 只能按 operation 的副作用状态进入 retry 或 reconcile；进程退出时停止接收新一轮任务，并在统一 drain 窗口内等待
当前轮次收敛，超时后由下一实例通过过期 lease 接续。

ready scan 同时折叠已完成 child 和 parent，立即发现 pending/过期 running；retry/reconcile 则按持久 `updated_at` 计算延迟，
避免重启后丢失等待边界。合法的 failed receipt 若声明 `side_effect=none` 且 `next_action=retry`，现在和 driver error 的 retry
一样可重新 claim 原 operation；此前 claim 路径只接受存在 `driver_error` 的 retry，导致这种 receipt 永久等待，现已由回归测试
封闭。

后续审查发现，最初的 ready 查询只检查最旧 1000 条非终态 workflow；如果整页都在等待 retry，后页已经 ready 的任务会饥饿。
现在同一稳定读事务按 `(updated_at, operation_id)` 做 keyset 分页，遍历全部页但只返回有界候选集；跨页测试用超过一页的延迟
记录证明后续 pending workflow 仍会被选择。driver/release selection 在 child 尚未 claim 前失败时，也不再依赖普通日志和下一次
固定轮询：workflow 持久保存 `dispatch_not_before`、指数退避计数、exact child operation ID，以及有界的类型化错误码和消息。
只有这些字段仍绑定当前 child 时才限制 ready；child 被其他实例 live claim、成功/失败 settle 或 workflow 推进都会清除过时退避，
因此旧 selector 不能覆盖较新的执行结果。旧数据库通过可重复的增量列迁移和一致性 trigger 升级，重启后仍保留等待边界。

精确提交 `56c65844d471c956af893d7151a6ea261131a48b`（tree
`54334c5b44e3aef27f99f0b2be9dc1ee3463a18a`）的 15,513,600-byte archive，SHA-256
`4775d5c52bddfafaef60e83db29a2946d24d594c39157e7f548eeb733d5814a7`，已在 Linux/macOS 通过 provider-driver
62/62、Control provider 23/23、协调器 3/3、target migration 2/2；Windows 为 54/54、23/23、3/3、2/2，三端五
crate 严格 Clippy 全过。Linux 同一 archive 另通过 16 份 Schema/48 组 fixture。正式 evidence 保留 Linux 收集脚本在运行中
被修改导致的 shell parse inconclusive，以及 macOS 非登录 PATH 与可重建临时构建目录占满磁盘的环境观察；两端随后均从
未改变的精确 archive 重跑通过。

公平性与选择退避实现提交 `b480b141ce15ac6b572c7742cc731f655fa6f4d5` 首次在 s69 暴露一项测试时钟假设：测试取两次
settle 中较晚的秒数加 29；Windows 较慢执行使两次 settle 跨秒，较早 workflow 因而恰好达到 30 秒 retry 边界并被产品正确
返回。该 archive 的 driver 54/54、Control 24/25，后续门禁未运行，这一失败没有被后续绿灯抹除。提交
`518d3f83760f577a31f117a739c37f0b240509d6` 改用最早 settle 秒数加 29，精确 15,564,800-byte archive（SHA-256
`d8216a9108eb3b57da50af369403eb3d7798c1fbf559a6d8b4348c5c555cee3d`）随后在 Linux/macOS 通过 driver 62/62、Control
25/25、协调器 3/3、target migration 2/2、dispatch-backoff migration 1/1；Windows 为 54/54、25/25、3/3、2/2、
1/1，三端严格 Clippy 全过。Linux 同一 archive 另通过 16 份 Schema/48 组 fixture 和产品治理；正式 evidence 同时保留
Windows 失败—修复关系及 macOS 非登录 SSH 缺 Cargo PATH 的环境观察。

这仍不是完整供应链闭环：当前 contract fixture 是合成分发物，尚未生成并实际签署 Wrangler 的真实完整安装树，也没有
真实透明日志/独立 witness 的采集与可用性运营，以及真实 Windows Service 安装演练。公共
release/vault/target/deployment API、三个完整 backend driver 和真实账户 evidence 也未完成。当前跨页扫描虽然正确，
但会在一个读事务内遍历全部非终态 workflow；大规模 catalog 仍需可索引 eligibility、负载证据和扫描时长指标。在这些完成并获得真实账户证据前，不能把当前 dispatcher
称作已部署的 provider 服务，更不能升级 backend 支持状态。

## 公共管理 API 的第一段边界

Control 的 human API 现已公开 provider-neutral 的 credential、installation、target 与 deployment 资源：凭据可创建、
列出、读取、轮换、吊销和读取不可变审计；installation/target 可注册和读取；application/environment 路径下可创建、
列出和读取 durable provider deployment。deployment 创建要求 `Idempotency-Key`，返回同一 `ProductOperation`、
`Location`、ETag、request ID 与 correlation ID；轮换和吊销要求强 `If-Match`。所有 provider 管理响应都带
`Cache-Control: no-store`。

凭据值只反序列化进 vault 写入路径；response model、operation、授权中间件和审计均没有明文字段。provider credential、
installation 与 target 是账户级 authority，因此当前全部要求平台管理员。deployment status 可按 application/environment
授权读取；但 target 尚未有 application/environment grant 模型，普通 deploy credential 不能选择任意账户发布，故开始
provider deployment 暂时也保持管理员权限。后续必须先建立 target assignment/grant，再把开始操作收窄到 deploy scope，
不能直接放宽路由权限。

target 注册不再相信调用者给出的一个版本字符串：版本必须是 `contractId@40-hex-commit` exact pin，必须存在于 Control
当前标准 registry，driver manifest 必须逐项声明相同 contract ID/version/digest，且 contract digest 必须与仓库锁一致。
读取 target 时重新从持久 manifest 验证并返回 contract ID 与 digest；旧 active target 若缺当前标准声明，会在升级时
disabled。SQLite insert/activation trigger 同时拒绝没有完整 SHA-256 声明的 active target。proposal 公共 fixture 继续保持
空 `supportedStandardVersions`，测试只为合成、已验证 release 显式加入当前声明，不能反向把 fixture 伪装成合规 driver。

这只是公共管理面的第一段，并不满足本 ADR 的完整验收。release/archive/authorization/transparency 安装 API 尚未公开；
installation/target 还没有 disable/delete 生命周期；列表只有有界 limit、没有 cursor；credential provision 和非 operation
资源创建还没有统一幂等 replay；CLI 与 Web 尚未接线。更关键的是尚无真实签署 release、真实透明日志/独立 witness、
三个完整 backend driver 或真实账户 lifecycle/conformance evidence。因此所有 backend 的 supported/compliant 状态保持不变。

## 后果

- 需要新增 provider-neutral deployment core 和 provider drivers，而不是继续把逻辑堆入 build adapter。
- build adapter 负责 artifact translation；deployment driver 负责账户、资源和发布，两者共享 target contract 但职责分离。
- CLI、control API、CI 和 Web UI 共享 deployment application service。
- 外部 provider deployment 成为正式安全边界和长期兼容责任。
- 当前 Cloudflare/Tencent/Deno 的“只生成文件”状态在 driver 完成前必须标为 experimental。

## 行业资料

- `https://github.com/secure-systems-lab/dsse/blob/master/protocol.md`
- `https://theupdateframework.github.io/specification/latest/`
- `https://docs.sigstore.dev/about/bundle/`
- `https://c2sp.org/tlog-proof`
- `https://c2sp.org/tlog-checkpoint@v1.0.0`
- `https://c2sp.org/signed-note@v1.0.0`
- `https://c2sp.org/tlog-cosignature@v1.0.1`
- `https://github.com/sigstore/protobuf-specs/blob/main/protos/sigstore_bundle.proto`
- `https://github.com/sigstore/protobuf-specs/blob/main/protos/sigstore_trustroot.proto`
- `https://developers.cloudflare.com/workers/wrangler/commands/`
- `https://developers.cloudflare.com/workers/versions-and-deployments/`
- `https://pages.edgeone.ai/document/edgeone-cli`
- `https://docs.deno.com/runtime/reference/cli/deploy/`
