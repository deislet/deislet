# Deislet 产品级执行路线

- 路线状态：Active
- 当前阶段：阶段 0
- 机器真源：[`roadmap.json`](roadmap.json)（每个工作项的状态、依赖、产物和验证方式都以它为准，本页不重复）
- 决策真源：[`docs/adr`](../adr/README.md)
- 标准锁定：[`standards/edge-canon.lock.json`](../../standards/edge-canon.lock.json)
- 用词：[术语表](../glossary.zh.md)

这六个阶段表示工程依赖，不是六个合规等级。任何 target 只有在一个完整、已发布 Edge Canon 版本的 mandatory suites 全部通过且认证仍有效时，才能称为 compliant；正式 supported 还要求实现处于维护和安全支持窗口。当前没有任何 target 通过认证，标准本身也仍是 proposal；各 target 的证据与认证状态只看[自动生成的目标状态](target-status.zh.md)。

| 阶段 | 状态 | 核心结果 |
|---|---|---|
| 0. 事实基线、标准治理与共享契约 | Active | ADR、独立 proposal、机器契约、registry、共享模型 |
| 1. 构建、开发、预览与部署骨架 | Queued | 一个 canonical artifact、provider driver、开发工具 |
| 2. 完整应用语义 | Queued | Node/SSR/ISR/存储/异步/Stateful Objects 在受支持 target 上一致 |
| 3. 自托管生产化 | Queued | isolate pool、安全默认、发布一致性、备份恢复、观测 |
| 4. 产品生命周期与控制台 | Queued | 统一 API/CLI/Web、Git、RBAC、数据生命周期 |
| 5. 完整认证与 GA | Queued | 两个受支持 target 有效认证、持续 canary、正式支持 |

## 产品边界

以下是已经拍板的定位，路线中的每个工作项都在这个边界内：

- **受支持的 target 只有两个**：自托管的 Deislet 与 Cloudflare Workers/Pages。Deno Deploy 与腾讯 EdgeOne 冻结：只在显式指定时构建，只生成无状态函数产物，不接受绑定，不做认证，不做新功能。
- **Cloudflare 部署**：构建与经控制面部署的路径已实现，只对照模拟的 `wrangler` 验证过；在 [`scripts/cloudflare-smoke.sh`](../../scripts/cloudflare-smoke.sh) 于真实账号跑通之前，口径是「已实现，未经真实账号验证」。见 [Cloudflare 运维文档](../operations/cloudflare.md)。
- **控制面单实例**：一个 SQLite 数据库，用 Litestream 做持续备份与恢复，不做多实例接管。
- **可信租户**：部署到同一安装中的应用代码视为可信；V8 isolate 是资源隔离，不是安全沙箱。平台是多应用，不是互不信任的多租户。
- **provider driver 的签名发布治理冻结**：只修缺陷，不加功能，只支持 Unix 主机。见 [provider-driver-governance.md](../operations/provider-driver-governance.md)。

## 进行中的工作项

以下编号对应 `roadmap.json` 中状态为 `active` 的工作项；进展和验证证据写在 `roadmap.json` 的 `verification` 字段和对应提交里，不在本页复述。

- `P0-SPEC-002`：逐能力族冻结 normative 语义、fixture 与 oracle。
- `P0-MODEL-001`：canonical ID、operation、artifact、binding 与 driver 的共享模型。
- `P1-BUILD-001`：从源码到 canonical artifact 的统一构建链。
- `P1-DEPLOY-001`：统一 provider driver API 与 CLI 生命周期（Cloudflare 真实账号验证、secret/config 下发、下线 API 待完成）。
- `P1-PLAT-001`：开发工具在 Linux/macOS/Windows 上的 CI 与确定性。
- `P2-NODE-001`：Node 24 LTS 子集与 npm 构建解析。
- `P2-DATA-001`：env/secrets/KV/cache/SQL/blob/queue/cron/service binding 的统一模型。
- `P3-SEC-001`：默认安全的生产配置、每节点身份与短期 capability。

`P1-ROUTE-001`（EdgeOne 路由表）因 EdgeOne 冻结而标为 `blocked`，不排期。

## 执行规则

- 一个工作项只有在产物存在且 `verification` 可重复时才能改成 `done`。
- 下游阶段不得绕过上游退出条件；确需并行时只能做不会冻结冲突接口的探索工作。
- 新发现必须进入阶段分析附录或新 ADR，再进入 `roadmap.json`，不能只留在聊天、commit message 或 issue 标题中。
- provider 官方 CLI/API 可作为 driver 的内部实现，但版本、输入 artifact、输出 identity 和错误必须被统一契约记录。
- 任何 mandatory test 的 skip、quarantine、flake 或语义 waiver 都阻断认证。
- 本页不写手工计数（测试数、schema 数、fixture 数等）；数字只由生成它的脚本或测试给出。

## 本地检查

```bash
python3 scripts/check-product-governance.py
```

该命令校验 `roadmap.json` 的结构、阶段顺序、依赖与完成项产物，并在 CI 中运行。
