# Basic Services

> Basic services that all platforms must support according to Edge Canon specification

---

## Overview

According to the [Edge Canon Basic Specification](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md), all compliant platforms must provide the following basic services:

1. **KV Storage** - Key-Value storage
2. **Cache API** - HTTP response caching

These services **do not need to be enabled in configuration**, and the deforge compiler will not perform platform compatibility checks for them, as all platforms are guaranteed to support them.

---

## KV Storage

### Usage

```typescript
export default async function handler(context: Context) {
  // Use directly, no configuration needed
  const kv = context.services.kv;

  await kv.put('user:123', JSON.stringify({ name: 'Alice' }));
  const data = await kv.get('user:123');

  return new Response(data);
}
```

### Platform Implementations

| Platform | Implementation | Value Size Limit |
|----------|---------------|------------------|
| Cloudflare | KV Namespaces | ≤ 25MB |
| Deno | Deno KV | ≤ 64KB |
| Tencent | EdgeKV | ≤ 2MB |
| Deislet | Denix KV | ≤ 10MB |

### Configuration (Optional)

Some platforms require specifying a namespace:

```json
{
  "services": {
    "kv": {
      "namespace": "MY_KV_STORE"
    }
  }
}
```

**Note**: `enabled: true` is not needed; this field is deprecated.

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

### Platform Implementations

All platforms support the standard Cache API.

---

## Difference from Extended Services

| Type | Examples | Configuration Requirement | Platform Check |
|------|----------|--------------------------|---------------|
| **Basic Services** | KV, Cache | No need to enable | ❌ Not checked |
| **Extended Services** | SQL Database, Object Storage, Queues | Need `enabled: true` | ✅ Platform support checked |

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

At compile time, it will check whether the target platform supports SQL Database. If not supported, it will fail with alternative suggestions.

---

## Compiler Behavior

### Basic Services

```bash
$ deforge build --vendor tencent

✓ Basic services (KV, Cache) are always available
✓ No platform compatibility check needed
```

### Extended Services

```bash
$ deforge build --vendor tencent

✗ Platform capability validation failed
Feature 'SQL Database' is not supported on EdgeOne Pages

Alternatives:
  1. Use external database service
  2. Use KV for simple data
  3. Deploy to Cloudflare (D1) or Deno (Postgres)
```

---

## Migration Guide

### Old Configuration (Deprecated)

```json
{
  "services": {
    "kv": {
      "enabled": true,     // ← No longer needed
      "namespace": "MY_KV"
    },
    "cache": {
      "enabled": true      // ← No longer needed
    }
  }
}
```

### New Configuration (Recommended)

```json
{
  "services": {
    "kv": {
      "namespace": "MY_KV"  // Only keep platform-specific config
    }
    // cache needs no configuration
  }
}
```

---

## References

- [Edge Canon Basic Specification](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md)
- [Edge Canon Platform Support Matrix](https://github.com/deislet/edge-canon/blob/main/PLATFORM_MATRIX.md)
- [deforge Platform Capabilities System](../crates/deforge-core/src/platform_capabilities.rs)
