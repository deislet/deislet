# Provider driver 实现入口

这里保存 provider 自动化实现的机器契约和来源锁，不保存账号凭证，也不把构建 adapter 当成部署支持。

- `official-tool-lock.json` 固定已审查官方工具的精确版本、npm registry tarball、SRI 与 SHA-256。
- `official-tool-lock.schema.json` 约束来源记录及服务端凭证传输结论。
- `contracts/v1/provider-driver.schema.json` 是完整 driver manifest 契约。只有 12 个生命周期操作全部实现并通过 contract/live tests 后，才可在这里增加某个 backend 的正式 manifest。
- `deis-provider-driver` crate 是同一契约的 Rust 执行边界：统一 request/receipt/error、不可绕过的 query-before-retry，以及不经 shell、清空继承环境、固定 executable digest、超时、进程组取消、输出限额和凭证脱敏。

当前锁定结论：

- Wrangler 4.129.0 官方支持 `CLOUDFLARE_API_TOKEN` 环境变量，可作为 Cloudflare 服务端 driver 的候选 transport；具体命令仍须逐个通过结构化输出和 live lifecycle 测试。
- EdgeOne 1.6.33 官方 CI 示例要求把 token 放进 `-t/--token`。这不满足统一契约的 `argvAllowed: false`，因此服务端 CLI 路线保持 blocked。不得以“供应商文档如此”为由弱化标准；应改用安全的官方 API/SDK，或等待/推动 CLI 提供环境、stdin、keyring 等安全 transport。

离线校验锁文件：

```bash
python3 scripts/check-provider-tool-lock.py
```

在升级评审时重新下载两个精确 tarball 并同时核对 SRI 和 SHA-256：

```bash
python3 scripts/check-provider-tool-lock.py --verify-downloads
```

下载校验只是来源证明，不是 driver 支持或 Edge Canon conformance 证明。
