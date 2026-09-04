# ADR-018：Linux 生产服务端与三平台开发工具

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-017
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/90897c134b649de4868aa41beebff1f23f346081/proposals/0001-unified-platform-contract/README.zh.md)

## 决策

Deislet 的生产服务端组件只正式支持 Linux。开发者 CLI、compiler、canonical artifact validator/provider packager 与 local development workflow 正式支持 Linux、macOS、Windows。

这不是把应用标准分成不同平台版本。Edge Canon 应用语义、artifact、Control API 和 provider deployment contract 仍然统一；区别只在 Deislet 产品哪些二进制能在哪些操作系统承担生产职责。

## 组件支持矩阵

| 组件/能力 | Linux | macOS | Windows |
|---|---|---|---|
| `deis` CLI | 正式支持 | 正式支持 | 正式支持 |
| Framework/compiler/canonical validator | 正式支持 | 正式支持 | 正式支持 |
| Provider packager/official CLI driver | 正式支持 | 正式支持 | 正式支持 |
| `deis dev` developer workflow | 正式支持 | 正式支持 | 正式支持 |
| Canonical artifact inspect/verify/sign | 正式支持 | 正式支持 | 正式支持 |
| Production control/proxy/runtime | 正式支持 | 不支持 | 不支持 |
| Production builder/store/queue/telemetry | 正式支持 | 不支持 | 不支持 |
| Production routing/Stateful Object/provider companion | 正式支持 | 不支持 | 不支持 |
| HA、rolling upgrade、backup/restore runbook | 正式支持 | 不支持 | 不支持 |

“不支持”表示项目不发布生产兼容、性能、安全响应与升级保证，不表示贡献者一定无法在该 OS 编译某个 crate。实验成功不能写成产品承诺。

## 为什么生产面只支持 Linux

产品级 runtime 与普通跨平台命令行工具不同，需要验证：

- cgroup v2 CPU/memory/process accounting 与 enforcement；
- namespace/seccomp/capability、no-new-privileges 与 sandbox；
- `/proc` CPU/thread/process observations；
- signal-driven drain、process groups 与 supervisor semantics；
- UID/GID、file ownership/mode、privileged port 与 least privilege；
- Unix socket、socket activation、reuseport 与 zero-downtime handoff；
- filesystem sync/rename/locking/crash recovery；
- systemd/OCI/Kubernetes health、restart 与 shutdown contract；
- networking、TLS key permissions、firewall 与 eBPF/diagnostics integrations。

在三种内核与服务管理体系上分别承诺同等级生产正确性，会把有限资源消耗在外围兼容层，并扩大故障与安全矩阵。选择 Linux 后仍需明确认证矩阵，不能把 Linux 当成一个完全同质的平台。

## Linux 正式支持基线

首个发布基线：

- OCI images：`linux/amd64`、`linux/arm64`；
- Host install：Ubuntu 24.04 LTS，amd64 首先，arm64 随发布矩阵验证；
- cgroup v2 与现代 Linux kernel/runtime capability checks；
- systemd unit 与 OCI/Kubernetes deployment forms；
- glibc-linked targets或明确的 musl 静态目标，不能让使用者猜 ABI；
- ext4/xfs/local durable filesystem 与经认证的 network/object/database backends；
- IPv4/IPv6、TLS/mTLS、graceful shutdown、upgrade、backup/restore、failure injection。

其他 distribution、kernel、libc、architecture 与 rootless/container runtime 只有进入公开 release matrix 并完成同一认证套件后才标为 supported。未经认证可以标 community-tested/experimental。

支持矩阵按产品版本记录：最低 kernel、cgroup mode、glibc/musl、CPU architecture、container runtime/Kubernetes、PostgreSQL/S3-compatible backend 等。升级版本不能静默抬高要求。

## Developer tools 正式支持基线

### Linux

- 当前受支持 LTS distributions/OCI dev container；
- amd64/arm64 release artifacts；
- Bash-compatible shell instructions，但 CLI 本身不依赖 shell 拼接业务命令。

### macOS

- Apple Silicon 为主要目标，Intel 是否继续支持由每版 release matrix 明示；
- 当前与前一受支持 major 的实际 runner/host 测试；
- Keychain credential storage、APFS case/Unicode 行为、launch/cancel/browser open；
- local Linux container backend 可通过受支持 container runtime 使用。

### Windows

- x64 为首个正式发布 target，arm64 按 release matrix 扩展；
- PowerShell 5.1 与当前 PowerShell 的安装、completion、stdin/stdout/exit-code 路径测试；
- Windows Credential Manager、long path、drive letter、UNC/path separators、case-insensitivity、file locking；
- `.exe`/`.cmd` provider CLI discovery 和 child process tree cancellation；
- WSL2/containers 是可选精确 Linux runtime 路径，不应成为基础 CLI/compiler 唯一运行方式。

现有 s69 Windows 10/PowerShell 5.1 可用于兼容回归，但 Windows 10 已过主流产品安全支持边界时，测试通过不能转化为 Deislet 对底层 OS 漏洞的安全保证。下载页分别写清 operating-system vendor support 与 Deislet compatibility status。

## Local development 模型

`deis dev` 是三平台正式产品，包含：

- framework adapter/native dev server integration；
- watch/rebuild/hot reload；
- Edge Canon request/context/binding emulator；
- embedded isolated KV/SQL/blob/queue/stateful data；
- cron/queue/alarm manual trigger；
- secrets/config overlay；
- local logs/traces/metrics；
- canonical build/validation on demand。

实现可以是：

1. 原生可移植的单进程 dev harness；
2. 受支持 Linux container 中运行 production-like runtime；
3. remote dev environment。

三种执行器使用相同 dev configuration schema 和 application API。产品明确显示 executor 与 parity status。原生 harness 若不能准确模拟 cgroup CPU limit、network sandbox、isolate concurrency、WebSocket drain、HA ownership 等，必须列出差异并在 deploy/preview 前运行真实目标验证。

本地便利不能演变为第二个标准实现：对 API response、error、consistency、ISR/Stateful Object/queue semantics 的模拟仍需通过 shared conformance；无法实现的功能明确拒绝或要求 container/remote executor，不能 no-op。

## 当前仓库证据

### 合理保留的 server-only Linux 依赖

- `deis-control`, `deis-store`, `deis-queue`, `deis-telemetry` 的 shutdown 直接使用 Unix `SIGTERM` API；
- `deis-proxy` 使用 `nix` UID/GID、fork/daemon、UnixListener 与 upgrade socket；
- deployment/runbooks 以 systemd、`/etc`、`/var/lib`、chmod/chown、journald 为基础；
- runtime CPU-time 读取 Linux `/proc/thread-self`，macOS/Windows 只能退化；
- proxy privileged port 与 capability/ownership 模型是 Linux/Unix 语义。

按本 ADR，这些不是必须全部抽象到 Windows service/launchd 的缺陷，但应有清晰 `cfg(target_os = "linux")`、compile error/message 与 crate packaging 边界。

### 必须修复的 client/tooling 缺口

- GitHub Actions `checks.yml` 的 workspace、demo 与静态检查全部只在 `ubuntu-latest`；
- compiler 的若干 artifact determinism/path/security tests 标为 `cfg(unix)`，Windows 没有等价证据；
- CLI 为保持 `--json` stdout 干净使用 Unix-only file-descriptor 重定向，非 Unix 路径会警告输出可能混杂；
- CLI 的 dev-dependencies 直接引入 `deis-queue`、`deis-telemetry` server crate，可能让 Windows client build 被 Unix server code 拖累；
- 文档大量只给 Bash、`/tmp`、chmod、`/etc` 路径，缺少 PowerShell/native path 指引；
- provider CLI verification 需覆盖 Windows `.cmd/.exe` 与非 shell process spawn；
- route/artifact path 已有部分 Windows separator 防护，但尚缺三平台同一 fixture/digest 的持续测试；
- watch/cancel/process tree/file locking/atomic replace/key storage 在 macOS/Windows 尚未形成发布级 e2e。

## Workspace 与 crate 边界

支持 Windows/macOS 不要求 `cargo build --workspace` 在这些系统编译生产 server。需要建立清晰包组：

- `client`: CLI、API clients、config、rendering；
- `build`: build core、framework adapters、artifact、provider packagers；
- `dev`: portable harness 与 optional container/remote executors；
- `server-linux`: control/proxy/runtime/builder/store/queue/telemetry/companions；
- shared wire/schema/domain crates 保持跨平台。

Client tests 不应通过 dev-dependency 链接整个 Linux server。需要真实服务的契约测试在 Linux integration job 启动进程/containers，客户端使用 schema fixtures/mock protocol 或远程 endpoint 测试 Windows/macOS。

Server crate 在非 Linux 若无法正确实现时应明确不构建/不发布，避免“能编译但缺少 SIGTERM/CPU enforcement/permissions”带来的假支持。

## Artifact 跨平台确定性

同一 source/build plan 的 canonical semantics 不得取决于构建机 OS：

- manifest path 统一使用 `/`，拒绝 `\` 等不可移植歧义；
- 拒绝在目标文件系统上碰撞的 case/Unicode normalization names；
- archive entry 排序、timestamp、owner/mode、compression 设置规范化；
- 文本换行是否规范化由 source snapshot/build plan 明确，不能由 Git checkout 默认值暗改；
- symlink/junction/reparse point、path traversal、reserved Windows names 有共同验证规则；
- executable bit 等语义由 manifest 明确，不从 Windows 缺失 mode 静默推断；
- file watcher rename/write burst debounce 后仍产生一致 build input snapshot；
- native dependency 明确 target triple，不能把 macOS/Windows binary 装入 Linux/provider artifact。

CI 对同一 fixture 在 Linux/macOS/Windows 产生 canonical artifact，比较 canonical manifest/digest；确有平台特定工具链输入时必须形成不同、被声明的 build plan，而不是偶然差异。

## Process 与 shell 兼容

所有工具调用使用 executable + argv + explicit cwd/env/stdin/stdout/stderr，不拼接 shell command string。只有用户显式配置的 build/install command 才进入 shell，并明确选择 shell：

- POSIX: documented shell and `-c` semantics；
- Windows: PowerShell/cmd selection and encoding；
- command string、argv list 两种 schema 不混淆。

覆盖：

- spaces/Unicode/metacharacters in paths；
- `.exe`, `.cmd`, shebang/shim discovery；
- Ctrl-C、timeout、cancel 对整个 child process tree；
- exit codes、signals/termination reasons；
- stdout/stderr binary/text encoding and structured JSON isolation；
- no secret in argv/process listing/error echo。

Provider official CLI driver 按 OS 锁定、校验相应 binary，不能下载 Linux CLI 后在 Windows shell 下偶然失败。

## 配置、凭证与路径

统一逻辑配置，物理默认路径遵循 OS：

- Linux：XDG/config/data/cache/state 或 system paths；
- macOS：Application Support/Caches/Keychain；
- Windows：AppData/LocalAppData/Credential Manager。

Project-local config 可跨平台提交，不写绝对 home/drive path。User overrides 显示来源与优先级。Credential abstraction 首选 OS secure store；headless CI 使用短期 token/OIDC/受限 file descriptor。权限检查在 Unix 看 mode/owner，在 Windows 看 ACL，不以 `chmod 600` 文案冒充 Windows 保护。

## 发布与升级

- 每个支持 target 发布独立 archive/package、SHA-256、signature、SBOM、provenance。
- CLI 自更新先验证 channel/target/signature，原子替换并可回退；Windows 处理 running `.exe` replacement。
- Package manager channels 可以提供 brew/winget/scoop/deb/rpm 等，但 binary tar/zip 是可验证基线。
- Server 发布 OCI multi-arch image 与 Linux packages；不发布暗示生产支持的 macOS/Windows server installers。
- `deis version --verbose` 显示 target、build provenance、API/artifact/standard compatibility。
- Upgrade tests 覆盖旧 CLI ↔ 新 server、新 CLI ↔ 受支持旧 server 的兼容窗口。

## CI 与实机认证

### 每次变更

- Ubuntu：format/lint/build/test server + client + build，security/audit，full demo/e2e。
- macOS：CLI/build/dev package compile、unit/integration fixtures、provider discovery、artifact determinism。
- Windows：同上，使用 PowerShell job 并实际执行 `.exe`；不能只 cross-compile。
- 三平台 canonical fixture outputs 比较。

### 定期与发布前

现有机器可形成补充矩阵：

- s17：本机 Ubuntu 24.04.4 开发/快速验证；
- s90/s91/s68：Ubuntu 24.04.4 多节点、HA、故障与升级；
- s67：macOS 26.5 native CLI/compiler/dev；
- s69：Windows 10/PowerShell 5.1 native CLI/compiler/dev compatibility。

远程实机任务必须固定 commit/artifact、记录 OS/architecture/tool versions、上传结构化结果并清理测试 credential/resources。共享主机的测试隔离、并发与端口/目录必须显式，不能依靠人工记忆。

CI provider deploy 使用 sandbox/test accounts 和短期 credential；常规 PR 可以 package/validate，真实外部部署在受控 nightly/release gate 执行。

## 文档与支持用语

统一四种状态：

- **Supported**：在公开矩阵内，CI/发布认证、文档、安全和升级承诺齐全。
- **Tested**：已在某组合运行，但未承诺全生命周期支持。
- **Experimental**：可试用，接口/行为可能变化，不用于合规或生产声明。
- **Unsupported**：不发布保证，问题可能只接受社区补丁。

每个下载、安装、组件 README、错误消息和诊断报告使用这些词，不以“portable”“cross-platform”概括不同组件。

## 验收条件

- Linux amd64/arm64 server images 通过 full conformance、HA、failure、upgrade、backup/restore 与 security suite。
- macOS/Windows 可以从干净机器完成 init/build/dev/preview/package/deploy/status/logs/rollback，不需要本机生产 server。
- Windows PowerShell 路径、JSON stdout、provider `.cmd/.exe`、cancel 与 credential store 有 e2e。
- macOS Keychain、Apple Silicon、case/Unicode filesystem 与 container/remote executor 有 e2e。
- 同一 portable fixture 在三平台产生相同 canonical artifact identity/semantics。
- Non-Linux client build 不编译 Linux server dependencies；Linux-only binary 不出现在 macOS/Windows release channel。
- Local dev parity gaps 明确显示，并由真实 target preview/conformance gate 覆盖。
- 文档和 `deis doctor` 准确输出 supported/tested/experimental 状态及缺失 production capability。
