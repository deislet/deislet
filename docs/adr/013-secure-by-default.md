# ADR-013：显式 Dev 模式与默认安全的 Production 模式

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-012
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/b59928d547db90b9a002bb893cef4e55ae762dc3/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

当前 Control 人类 API 认证为兼容旧安装默认关闭。节点和 proxy 使用整个 fleet 等价的共享凭证；持有该凭证可能访问 assignments、bundle、路由、节点管理和环境 secrets。Runtime data port 的隔离又较多依赖私网与防火墙。

Loopback、容器私网或“位于反向代理之后”都不是可靠身份边界。错误配置会把开发便利直接带入生产。

产品负责人决定：只有显式 dev 模式允许受限本地便利；所有其他模式安全默认开启，缺少必要安全配置时不能报告 production-ready。

## 两个正交维度

不要把部署规模与安全模式混为一谈：

| 维度 | 值 | 含义 |
|---|---|---|
| Runtime mode | `dev` / `production` | 身份、TLS、listener、审计和安全门禁 |
| Persistence mode | `embedded` / `production` | SQLite/local 或可插拔 HA 后端 |

因此：

- `production + embedded` 是允许的安全单机部署，只是不具备在线 HA。
- `dev + embedded` 是默认本地开发。
- `production + production persistence` 是 HA 参考部署。
- 不允许 `dev` 作为远程共享或生产环境的捷径。

## Production 安全基线

非 dev 模式必须：

- Control API authentication enabled。
- 独立 human、machine 和 service identities。
- Management traffic TLS/mTLS。
- 最小权限 authorization。
- Secret encryption and key persistence。
- Credential expiry/rotation/revocation。
- Audit enabled and writable。
- Data/management listeners separated。
- Production readiness checks pass。

缺一项时服务可以启动到 diagnostic/not-ready 状态，但不能接受正常生产流量或显示健康。

## 首次初始化与 Bootstrap

建议流程：

1. `deis platform init --mode production` 在本地安全终端执行。
2. 生成 platform ID、root key reference、内部 CA 和一次性 bootstrap credential。
3. 只显示一次 bootstrap secret，并明确保存要求。
4. 使用它创建第一个管理员或绑定外部 identity provider。
5. Bootstrap credential 成功使用后立即失效；未使用也有短期限。
6. 数据库只保存 salted hash、ID、scope、expiry 和状态。

非交互部署可以通过受保护文件 descriptor/stdin/KMS secret 完成 bootstrap，不能在命令行 argv 或 cloud-init 公共日志中暴露。

## 身份拆分

### Human identity

- User/account identity。
- Web session 或个人 API token。
- Organization/application/environment membership。
- RBAC/permission scopes。
- MFA/SSO 可以后续接入，但数据模型不应阻止。

### Node identity

- 每 runtime/proxy/store/queue/telemetry/control instance 独立 identity。
- mTLS certificate/SPIFFE-like URI 或等价标准 identity。
- Certificate short lifetime and automated rotation。
- Node registration 使用一次性、scope-restricted enrollment token。
- 单节点撤销不影响整 fleet。

### Provider driver identity

- 每 provider installation 独立 service credential。
- Scope 到 account/project/environment 和资源操作。
- 不与应用 runtime secret 共享。
- 解密只发生在 driver operation 生命周期。

### Runtime binding capability

- Scope 到 tenant、app、environment、version、binding、allowed operations。
- 短期、可撤销、audience-restricted。
- Store/queue/service binding 每次校验。
- Runtime compromise 不能获得资源管理或其他 binding 权限。

## 服务权限边界

### Proxy

只允许：

- 获取签名/版本化 routing snapshot。
- 解析 TLS/domain route。
- 调用指定 runtime version data plane。
- 上报 proxy telemetry。

不允许：

- 下载所有 assignment plaintext secrets。
- 调用 node management/inspector。
- 管理 store/queue/provider resources。
- 注册/撤销其他 nodes。

### Runtime

只允许：

- 读取分配给自己的 version/artifact。
- 获取当前 version 的最小 secret/binding capabilities。
- 调用对应 data resources。
- 上报 invocation telemetry。

### Control

- 管理 desired state、identities、deployments 和 credential references。
- 不应为普通查询解密应用 secret。
- 高权限操作使用 step-up/re-auth 和审计。

### Store/Queue/Stateful services

- 不信任 app/environment headers 作为身份。
- 从认证 principal/capability 得到 tenant/binding scope。
- 管理 API 与数据 API 分离。

## Listener 与网络

- Human Control API、node management、runtime data、store data、admin/metrics 使用独立 listener 或明确的 protocol/auth boundary。
- Management endpoints 不在公开 proxy route 上。
- Runtime data listener 即使在私网也需要 machine auth。
- Metrics 不暴露 secret、token 或高基数敏感 key。
- Egress policy/SSRF protection 是 runtime 和 provider driver 的独立边界。
- Unix socket 可以用于同机内部连接，但仍映射明确 identity。

## Dev 模式

Dev 只用于明确本地进程：

- 默认 bind loopback/Unix socket。
- 每次启动生成 ephemeral process identity/token。
- 退出后 credential 失效。
- 数据和 credential 放在明确 dev directory。
- UI/CLI 明显显示 DEV / NOT PRODUCTION READY。
- 尝试 bind non-loopback、连接远程 node 或启用公网域名时拒绝，除非切换 production mode。
- Dev 可以简化首次登录，但内部 service calls 仍尽量走同一 auth code path，避免生产路径未测试。

不能根据 bind address 自动推断 dev；mode 必须显式。

## API Token 与 Session

API token：

- ID + secret，数据库只保存 hash。
- Explicit scopes、resource selectors、audience、created/last-used/expiry。
- 支持 rotation overlap 和 immediate revoke。
- 高权限 token 强制到期。
- Token 值只显示一次。

Web session：

- HttpOnly、Secure、SameSite cookie。
- CSRF token/origin validation。
- CORS 默认 same-origin/deny。
- Session fixation protection and rotation after privilege change。
- Idle/absolute expiry。
- Sensitive actions re-auth。

CLI：

- 使用 system keyring 或 mode 0600 credential file。
- 不在 shell history/argv 输出 secret。
- 支持明确 profile/account/target，防止隐式误操作。

## Secret 加密

建议 envelope model：

```text
platform root/KMS key
  → wraps tenant/environment data encryption key
      → encrypts versioned secret values
```

要求：

- 每 tenant/environment key isolation。
- Key ID/version 与 ciphertext 一起存储。
- Root key 不与 control DB/bundle backup 同位置明文保存。
- Online rotation and rewrap。
- Secret value 不进入 logs、audit payload、artifact、CLI argv 或 provider temp output。
- Runtime 只接收当前 version 所需 secret，生命周期有界。
- Backup/restore drill 包含 key 可用性验证。

## Readiness 门禁

Production `/ready` 或 control status 至少检查：

- API auth enabled。
- 没有默认/空/弱 bootstrap credential。
- Bootstrap 已消费或安全到期。
- Internal CA/node cert 有效且可轮换。
- 没有继续依赖 fleet-wide legacy secret。
- Management listeners 使用 TLS/mTLS 或受认证 Unix socket。
- Root/envelope key 可恢复且 backup 状态有效。
- Audit sink writable。
- Provider credentials 可解密、未过期且 scope 可检查。
- Data plane capabilities short-lived and issuer healthy。
- Clock skew within token/lease tolerance。

检查失败使用结构化 reason，不能只写日志后仍返回 ready。

## Break-glass

- 只允许本机/受控管理通道触发。
- 需要离线保存的独立 emergency credential 或多方流程。
- 有短时效、最小 scope 和强告警。
- 所有操作追加 audit，不允许关闭审计掩盖使用。
- 使用后强制轮换相关 credential 并生成 incident record。
- Break-glass 不能成为日常绕过 auth 的配置开关。

## 审计

必须审计：

- Login/token/session/identity lifecycle。
- Bootstrap and break-glass。
- Permission/membership change。
- Secret create/read-metadata/update/delete/rotate（不记录 value）。
- Node enroll/revoke/cert rotate。
- Provider credential and deployment operations。
- Resource bind/import/delete/purge/export。
- Version activate/gradual step/rollback。
- Inspector/debug/data access。

Audit event 带 actor、subject、action、resource、result、request/operation ID、time 和来源。Production audit sink 写失败时，高风险管理操作 fail closed。

## 旧安装迁移

1. 启动时检测 `api_auth_enabled=false`、共享 node token 和未分离 listener。
2. 输出结构化迁移报告，不继续静默兼容。
3. 提供命令生成管理员、内部 CA、每节点 enrollment 和新 credentials。
4. 支持短期双凭证窗口完成滚动迁移。
5. 记录 legacy credential 最后使用者和时间。
6. 完成后撤销 shared token、启用 readiness enforcement。
7. 明确版本后删除 legacy bypass；延期必须有显式危险确认和醒目标记。

## 测试

- Production defaults 无配置时不能以 insecure-ready 启动。
- Dev 无法意外公开监听。
- 每节点 revoke/rotation 不影响其他节点。
- Proxy credential 无法访问 assignments secrets/management。
- Runtime capability 不能跨 app/env/binding/operation。
- Listener 和管理/data API 隔离。
- Bootstrap one-time/expiry/concurrent use。
- Session CSRF/CORS/cookie/security。
- Secret/credential 不进入 logs、audit、argv、artifact、crash dump。
- Envelope key rotate/backup/restore。
- Audit fail-closed for sensitive operations。
- Legacy migration and final removal。
- Proxy auth、SSRF、DNS rebinding、redirect、WebSocket egress regressions。

## 后果

- `api_auth_enabled=false` 不再是 production 默认值。
- 现有 fleet shared credential、proxy 权限和 assignment secret 传递需要重构。
- Control identity/RBAC、internal CA/cert rotation、secret envelope 和 readiness 成为产品级子系统。
- Dev flow 需要显式、易用的一键启动，避免安全默认损害本地体验。
- 旧部署需要受控迁移期，但不无限维持不安全行为。
