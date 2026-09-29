# Logical resources

The standard HTTP handler receives exactly four context fields:
`request`, `env`, `params`, and `waitUntil`. A resource is therefore reached
through a declared logical name in `context.env`; `context.services` and
`context.bindings` are not portable APIs.

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

The name is the application ABI. Provider resource IDs, accounts, regions,
connection strings, and secret values belong to the environment binding plan,
not to source or the canonical artifact. A deployment driver creates or
imports the provider resource and binds that same logical name.

Deislet currently implements named `kv`, `database`, `blob`, `queue`, and
`service` bindings. Their cross-provider interfaces are still Draft because
Edge Canon's `logical-resource-bindings` capability family is not frozen.
Building or running one Deislet implementation is therefore not a conformance
claim; current evidence and support state comes only from the
[registry-derived target view](../../../docs/product/target-status.zh.md).

The Cache API is a Web API rather than a context convenience. Use
`caches.default` or `await caches.open(name)`. Its current Deislet behavior is
in-process and isolate-local; no application may assume cross-node persistence
until a selected standard version says so.

The runtime's native `context.services` table is below the generated bridge
and is not part of the standard context; the build refuses a project that
turns on an unnamed `services` flag. Use a declared binding such as
`context.env.SESSIONS` instead of an unnamed KV, and `caches.default` for the
cache. The standard context has no alias for either.
