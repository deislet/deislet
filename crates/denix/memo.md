Denix Deno Infrastructure Memo

当前结论
- 已完成: 模块加载（ModuleLoader + DB 读取，多文件 ESM）、TypeScript 转译（deno_ast）、权限映射（deno_permissions）。
- 未完成: Inspector / CDT、ResourceTable 配额与 OpMetrics、快照启动、完整 Web API 补全（deno_console / deno_url 等）。

实现位置（核心）
- /ext/deislet/denix/src/runtime/loader.rs
- /ext/deislet/denix/src/runtime/isolate.rs
- /ext/deislet/denix/src/app/types.rs

近期变更摘要
- DbModuleLoader 从 AppRepo 读取模块，默认入口为 app.ts，可通过 entry_point 指定。
- 使用 deno_ast 标准转译 TS/TSX/JSX，替代正则剥离。
- 权限从 AppConfig 映射到 deno_permissions，避免 allow_all。

后续建议优先级
1) Inspector / CDT
2) ResourceTable 配额 + OpMetrics
3) Web API 补全（deno_console / deno_url / Timers 等）
4) 快照启动
