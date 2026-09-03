# Deislet 产品级执行路线

- 路线状态：Active
- 当前阶段：阶段 0
- 机器真源：[`roadmap.json`](roadmap.json)
- 决策真源：[`docs/adr`](../adr/README.md)
- 标准锁定：[`standards/edge-canon.lock.json`](../../standards/edge-canon.lock.json)

这六个阶段表示工程依赖，不是六个合规等级。任何后端只有在一个完整、已发布 Edge Canon 版本的 mandatory suites 全部通过且认证仍有效时，才能称为 compliant；正式 supported 还要求实现处于维护和安全支持窗口。

| 阶段 | 状态 | 核心结果 |
|---|---|---|
| 0. 事实基线、标准治理与共享契约 | Active | ADR、独立 proposal、机器契约、registry、共享模型 |
| 1. 构建、开发、预览与部署骨架 | Queued | 一个 canonical artifact、统一 provider drivers、三平台开发工具 |
| 2. 完整应用语义 | Queued | Node/SSR/ISR/存储/异步/Stateful Objects 跨后端一致 |
| 3. Deislet 生产化 | Queued | isolate pool、安全默认、发布一致性、HA、观测 |
| 4. 产品生命周期与控制台 | Queued | 统一 API/CLI/Web、Git、RBAC、数据生命周期 |
| 5. 完整认证与 GA | Queued | 三个一等后端有效认证、持续 canary、正式支持 |

## 当前已完成

- `P0-DEC-001`：22 份决策和阶段事实基线已迁入仓库；ADR-022 局部取代 ADR-005 的套餐基线，要求所有参考供应商最低免费/入门档都达到同一标准保证。
- `P0-SPEC-001`：Edge Canon `edge-canon.next` proposal 与单一标准契约已建立。
- `P0-CONF-001`：公开 registry、证据成熟度和认证不变量已建立。
- `P0-ROAD-001`：路线、标准锁定、跨文件校验和 CI 门禁已建立。

## 当前进行中

- `P0-SPEC-002`：已建立 15 个能力族 × 8 个语义维度的需求注册表；`web-fetch-events` 已形成 20 条候选条款和 11 个 provider-independent 草案用例。最低资源保证已固定采用所有参考供应商最低免费/入门档共同基线，但具体数值、计量和超限行为仍为 `pending`；执行 harness 仍为 `planned`，其余 14 个能力族尚未开始，故本项继续保持 `active`。
- `P0-MODEL-001`：已建立 canonical identity、artifact、binding snapshot、operation、provider driver 和 identity migration 六份 Draft schema，配套 12 个正反 fixture 与可执行状态图校验。Rust/SQLite/API 已完成 application/environment/version/deployment 的增量 UUIDv7、共享强类型、旧库回填和旧 writer 兼容，四类 identity 一经分配均拒绝擦除或改绑到另一合法 UUIDv7；`CanonicalTenantId` 经 Control push/assignment 传输，节点成对 fail-closed 持久化并拒绝重绑/擦除；runtime placement 与 storage authority 已分离。Control 内部又新增持久 migration/operation ID、规范且确定的 catalog inventory、绑定 canonical app/environment 归属的 binding ID、application revision 与 SQLite catalog fence：重复计划幂等，过期 token/revision、错误 manifest owner、跨应用引用、历史 binding 歧义和 fence 后的新旧 writer 均被拒绝，失败不留下部分 mapping。Deislet 后端现为逻辑 `v1/<app UUIDv7>/<environment UUIDv7>` 定义独立、版本化且可逆的单路径组件物理编码 `ct1_<app UUIDv7>_<environment UUIDv7>`；binding 后缀只用永久 UUIDv7。store 的 KV/SQL/blob、snapshot/restore/orphan scan 与 queue reopen 已验证 canonical 迁移目标和 legacy 数据相互隔离。该物理编码不是标准 wire identity，live request parser 仍拒绝逻辑和物理 canonical key；外部组件仍为 `pending`，内部记录也尚不是完整公共 Operation API，store/queue writer fence、token/binding cutover 均未完成，故本项继续保持 `active`。
- `P0-EVID-001`：已把 Edge Canon 锁定 commit 的 conformance registry 原样保存，并用 SHA-256 绑定到 standard lock；确定性生成器产出带 JSON Schema 的机器状态和 Markdown 人类视图，CI 会拒绝 snapshot、lock、proposal/compliance/support 不变量或生成结果漂移。产品入口不再手工复制当前支持结论。CLI/Web 尚未读取同一 JSON，compiler 的 `PlatformCapabilities::is_supported` 也仍把 Deislet packager 实现可用性误称为平台支持，故本项保持 `active`。
- `P0-CI-001`：已按 GitHub 官方 Node 24 版本迁移 checkout、setup-node、setup-python、cache 和 upload-artifact。`e80c29f` 的单一 workspace job 在 60 分钟上限取消；提升后，`3d11a09` 的 run [`33779883077`](https://github.com/deislet/deislet/actions/runs/33779883077) 又在 120 分钟整取消，Tests 单步已运行约 94 分 46 秒。拆分后的首轮 run [`33792126645`](https://github.com/deislet/deislet/actions/runs/33792126645) 已证明 compiler/CLI、control/services、demo 和治理分片可独立成功，但 runtime 分片发现两项真实 CI 假设：package tests 不会构建另一个 package 的 `deis-proxy` 二进制；GitHub runner 的 `localhost` 首选 `::1`，旧 hostname-policy 用例却只监听 IPv4。workflow 现显式构建被集成测试启动的 proxy，治理脚本锁定此前置；测试 echo server 改为只服务 loopback 的双栈 listener。该 run 的失败必须保留，仍须等待修复后完整 checks 全绿且 Node 20 annotation 为零才能标为 `done`。

## 紧接着执行

1. `P0-SPEC-002`：先完成 `web-fetch-events` 的最低资源保证、依赖类型和三后端可执行 harness，再按其闭环逐项完成其余 14 个 mandatory 能力族；`pending` 是发布阻断项。
2. `P0-MODEL-001`：在已完成 ID 分配、canonical tenant 传输、catalog inventory/revision fence、runtime 权限入口拆分和 Deislet 物理 canonical namespace 编码的基础上，下一步为 KV/SQL/blob/queue/telemetry/provider 实现可证明的外部 inventory 与 storage-writer fence；之后才可启用 canonical-first dual-read，执行 copy/verify/cutover、旧 token 撤销与 rollback。catalog fence 不能冒充 storage fence，后端物理 key 也不能冒充标准逻辑身份或 capability subject。
3. `P0-EVID-001`：把 CLI/Web 接到现有 registry 派生 JSON，并把 compiler 的 target implementation check 从标准支持/供应商 capability 术语中拆出；adapter 或 artifact 只能形成 evidence maturity，不能升级合规或支持。
4. `P0-CI-001`：观察加入 runtime proxy 构建前置和双栈 localhost fixture 后的完整 checks run；全部 package tests、static gates、demo 和治理 jobs 都成功且 Node 20 runtime annotation 为零后关闭本项。

## 执行规则

- 一个工作项只有在产物存在且 `verification` 可重复时才能改成 `done`。
- 下游阶段不得绕过上游退出条件；确需并行时只能做不会冻结冲突接口的探索工作。
- 新发现必须进入阶段分析附录或新 ADR，再进入 `roadmap.json`，不能只留在聊天、commit message 或 issue 标题中。
- provider 官方 CLI/API 可作为 driver 的内部实现，但版本、输入 artifact、输出 identity 和错误必须被统一契约记录。
- 任何 mandatory test 的 skip、quarantine、flake 或语义 waiver 都阻断认证。

## 本地检查

```bash
python3 scripts/check-product-governance.py
```

该命令在 Linux、macOS 和 Windows 的 Python 3 上使用相同逻辑，并在 CI 中运行。更细的 work item、依赖、产物和退出条件以 `roadmap.json` 为准。
