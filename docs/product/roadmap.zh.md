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
- `P0-MODEL-001`：已建立 canonical identity、artifact、binding snapshot、operation、provider driver 和 identity migration 六份 Draft schema，配套 12 个正反 fixture 与可执行状态图校验。Rust/SQLite/API 已完成 application/environment/version/deployment 的增量 UUIDv7 分配、共享强类型验证、存量回填、唯一索引、非法 SQL 写入拒绝和旧 writer 兼容触发器；CLI 对旧 Control 保持字段缺省兼容，但拒绝伪 canonical ID。tenant/binding/capability、持久 migration operation 与受 fencing 的 cutover 尚未迁移，故本项继续保持 `active`。
- `P0-CI-001`：已按 GitHub 官方 Node 24 版本迁移 checkout、setup-node、setup-python、cache 和 upload-artifact；等待完整 checks run 验证后才能标为 `complete`。

## 紧接着执行

1. `P0-SPEC-002`：先完成 `web-fetch-events` 的最低资源保证、依赖类型和三后端可执行 harness，再按其闭环逐项完成其余 14 个 mandatory 能力族；`pending` 是发布阻断项。
2. `P0-MODEL-001`：在已完成 ID 分配与旧库回填的基础上，实现完整 inventory 和 canonical tenant/binding 双读，随后验证受 fencing 的 copy/cutover、旧 token 撤销与 rollback。
3. `P0-EVID-001`：删除手工维护的“支持”结论，让 README、CLI、Web 和矩阵从 registry 生成。
4. `P0-CI-001`：观察 Node 24 action revisions 的首个完整 checks run，确认成功且不再产生 Node 20 runtime annotation 后关闭本项。

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
