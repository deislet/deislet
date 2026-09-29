# 逻辑资源

标准 HTTP handler 的 context 只有 `request`、`env`、`params`、`waitUntil`
四个字段。资源必须按声明的逻辑名称从 `context.env` 取得；
没有 `context.services` 与 `context.bindings`。

```json
{
  "bindings": {
    "SESSIONS": { "kind": "kv" },
    "PRIMARY": { "kind": "database" },
    "UPLOADS": { "kind": "blob" },
    "JOBS": { "kind": "queue" }
  }
}
```

```typescript
export default async function handler(context: Context): Promise<Response> {
  await context.env.SESSIONS.put('user:123', JSON.stringify({ name: 'Alice' }));
  const data = await context.env.SESSIONS.get('user:123');
  return new Response(data);
}
```

逻辑名称是应用 ABI。供应商资源 ID、账号、地域、连接串和 secret value 属于环境级
binding plan，不能进入源码或 canonical artifact。部署 driver 负责创建或导入供应商资源，
并把同一个逻辑名称绑定到它。

Deislet 当前实现了具名 `kv`、`database`、`blob`、`queue`、`service` binding；但
Edge Canon 的 `logical-resource-bindings` 能力族尚未冻结，因此这些跨平台接口仍是 Draft。
能在 Deislet 构建或运行不等于已经合规；当前证据与支持状态只看
[registry 派生目标视图](../../../docs/product/target-status.zh.md)。

Cache API 是 Web API，不是 context 便利字段。使用 `caches.default` 或
`await caches.open(name)`。Deislet 当前实现是进程内、按 isolate 隔离；在选定标准版本
明确以前，应用不能假设它跨节点持久。

没有无名命名空间：应用能够到的每个 KV、数据库、对象存储或队列都是声明过的绑定（如
`context.env.SESSIONS`），缓存用 `caches.default`。任何类型的 handler 读取
`context.services` 或 `context.bindings`，构建都会拒绝。
