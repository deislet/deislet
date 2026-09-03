# Basic Services

> Baseline services required by the selected Edge Canon standard version

---

## Overview

According to the [Edge Canon Basic Specification](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md), all compliant platforms must provide the following basic services:

1. **KV Storage** - Key-Value storage
2. **Cache API** - HTTP response caching

These services **do not need to be enabled in configuration** and use the
common build path. Backend conformance is proven by the Edge Canon suite; the
compiler does not infer it from a target name.

---

## KV Storage

### Usage

`kv.put` stores bytes: a string is UTF-8 encoded, and an `ArrayBuffer` or
`ArrayBufferView` (e.g. `Uint8Array`) is stored as given. Non-text content
such as an image or a compressed frame does not need to be turned into a
string first.

```typescript
export default async function handler(context: Context) {
  // Use directly, no configuration needed
  const kv = context.services.kv;

  await kv.put('user:123', JSON.stringify({ name: 'Alice' }));
  const data = await kv.get('user:123'); // read back as text by default

  return new Response(data);
}
```

`kv.get`'s second argument, `type`, chooses how the stored bytes are read
back: `"text"` (the default, UTF-8 decoded), `"json"` (decoded then
`JSON.parse`d), or `"arrayBuffer"` (the raw bytes, no decoding — the right
choice for non-text content). These three names match other platforms; only
`"stream"` is not available — a read on this platform always returns the
whole value at once, there is no streaming read behind it, and asking for
`"stream"` throws an error saying so rather than silently falling back to
text.

```typescript
const image = await kv.get('avatar:123', { type: 'arrayBuffer' });
const parsed = await kv.get('user:123', { type: 'json' }); // same as JSON.parse(await kv.get(...))
```

`kv.put` also accepts `expirationTtl` (seconds until the entry expires) and
`metadata` (stored alongside the value and read back separately with
`kv.getWithMetadata()`; it does not count against the value's own size).

### Standard and target status

The selected Edge Canon release defines the portable KV behavior and minimum
limits. A vendor product name or a value-size number copied into this guide is
not proof that a backend implements those semantics. Current backend evidence,
certification and support status comes from the
[registry-derived view](../../../docs/product/target-status.zh.md).

### Configuration

```json
{
  "services": {
    "kv": { "enabled": true }
  }
}
```

`enabled` is the only field. `namespace` and `connectionString` were here until
2026-08-30 and were removed because **nothing in the workspace ever read
either**. The bundle manifest records services as booleans, and a tenant is
already uniquely `(application, environment)`, so an application's KV is its
own and has nothing to be named apart from.

An application that wants two logical stores prefixes its keys and enumerates
one prefix with `kv.list({ prefix })`. Where a boundary the platform actually
enforces is wanted rather than a way to organise keys, that is a second
environment or a second application - the tenant boundary deis-store checks on
every call.

---

## Cache API

### Usage

```typescript
export default async function handler(context: Context) {
  // Use directly, no configuration needed
  const cache = context.services.cache;
  const url = new URL(context.request.url);

  const cached = await cache.match(url);
  if (cached) return cached;

  const response = new Response('Hello World');
  await cache.put(url, response.clone());

  return response;
}
```

### Standard and target status

Cache is mandatory in the selected standard; that requirement is not evidence
that every backend currently conforms. The compiler deliberately makes no
target-name inference here. Conformance is established only by the shared
suite and registry.

---

## Difference from Extended Services

| Type | Examples | Configuration Requirement | Build Check |
|------|----------|--------------------------|---------------|
| **Basic Services** | KV, Cache | No need to enable | ❌ Not checked |
| **Extended Services** | SQL Database, Object Storage, Queues | Need `enabled: true` | ✅ Current target-packager mapping checked |

### Extended Service Example

```json
{
  "services": {
    "database": {
      "enabled": true,  // ← Required
      "binding": "DB"
    }
  }
}
```

At compile time, Deislet checks whether its current target packager has a SQL
Database mapping. A missing mapping fails the artifact build. This is not a
vendor capability, conformance, or support result; use the
[registry-derived status](../../../docs/product/target-status.zh.md) for those.

Once enabled, `context.services.database` offers more than statement-by-statement
`execute`/`query`: two ways to combine several statements into one call.

```typescript
// batch: every statement is decided up front, one round trip, all-or-nothing;
// there is no point to read a result and branch on it, and a row-returning
// SELECT is refused.
await database.batch([
  { sql: "UPDATE items SET stock = stock - 1 WHERE id = ? AND stock > 0", params: [id] },
  { sql: "INSERT INTO orders (item, buyer) VALUES (?, ?)", params: [id, buyer] },
]);

// transaction: the callback can read, decide, and write, all through one
// connection pinned to it until the callback returns; that connection is a
// scarce resource, so the transaction carries a deadline.
const total = await database.transaction(async (tx) => {
  const [row] = await tx.query("SELECT balance FROM accounts WHERE id = ?", [id]);
  await tx.execute("UPDATE accounts SET balance = ? WHERE id = ?", [row.balance - 10, id]);
  return row.balance - 10;
});
```

Not sure which to reach for: if every statement is decided before anything
runs, use `batch`; use `transaction` only when a later statement depends on
what an earlier one read.

`context.services.queue` changed the same way: `send()` now carries bytes
rather than only text — a string is UTF-8 encoded, an object is
`JSON.stringify`-ed first, and an `ArrayBuffer`/`ArrayBufferView` is sent as
given. On the consumer side, `message.body` is the text exactly as it was
sent, but when the body is not valid UTF-8, `message.body` is `null` (not an
empty string, not mangled text) — such a message can only be read with
`message.bytes()`; calling `message.json()` on it throws, naming the reason,
rather than trying to parse text that was never there.

---

## Compiler Behavior

### Basic Services

```bash
$ deis-build build --vendor tencent
# No capability message is printed. KV and Cache use the common artifact path;
# their deployed semantics are checked by conformance tests, not this build.
```

### Extended Services

```bash
$ deis-build build --vendor tencent

✗ Target artifact implementation check failed
Project cannot yet be compiled by Deislet's EdgeOne Pages target packager:
1. Feature 'SQL Database': no current target implementation
```

---

## Migration Guide

### Old Configuration

```json
{
  "services": {
    "kv": {
      "enabled": true,
      "namespace": "MY_KV"      // ← removed; no component ever read it
    },
    "cache": {
      "enabled": true           // ← not needed: the cache is a basic feature
    }
  }
}
```

### Now

```json
{
  "services": {
    "kv": { "enabled": true }
  }
}
```

Unknown keys are still accepted rather than rejected, so a project file
carrying the old two still parses - those fields simply have no effect, and
never had one.

---

## References

- [Locked Edge Canon standard and registry](../../../standards/edge-canon.lock.json)
- [deis-build target artifact coverage](../../deis-build-core/src/target_artifact_coverage.rs)
