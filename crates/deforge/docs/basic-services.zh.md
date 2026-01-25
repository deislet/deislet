# 基础服务 (Basic Services)

> Edge Canon 规范要求所有平台必须支持的基础服务

---

## 概述

根据 [Edge Canon 基础规范](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md)，所有兼容平台必须提供以下基础服务：

1. **KV Storage** - 键值存储
2. **Cache API** - HTTP 响应缓存

这些服务**无需在配置中启用**，deforge 编译器不会对它们进行平台兼容性检查，因为所有平台都保证支持。

---

## KV Storage

### 使用方式

```typescript
export default async function handler(context: Context) {
  // 直接使用，无需配置
  const kv = context.services.kv;

  await kv.put('user:123', JSON.stringify({ name: 'Alice' }));
  const data = await kv.get('user:123');

  return new Response(data);
}
```

### 平台实现

| 平台 | 实现方式 | 值大小限制 |
|------|---------|-----------|
| Cloudflare | KV Namespaces | ≤ 25MB |
| Deno | Deno KV | ≤ 64KB |
| Tencent | EdgeKV | ≤ 2MB |
| Deislet | Denix KV | ≤ 10MB |

### 配置（可选）

某些平台需要指定 namespace：

```json
{
  "services": {
    "kv": {
      "namespace": "MY_KV_STORE"
    }
  }
}
```

**注意**：无需 `enabled: true`，该字段已废弃。

---

## Cache API

### 使用方式

```typescript
export default async function handler(context: Context) {
  // 直接使用，无需配置
  const cache = context.services.cache;
  const url = new URL(context.request.url);

  const cached = await cache.match(url);
  if (cached) return cached;

  const response = new Response('Hello World');
  await cache.put(url, response.clone());

  return response;
}
```

### 平台实现

所有平台均支持标准 Cache API。

---

## 与扩展服务的区别

| 类型 | 示例 | 配置要求 | 平台检查 |
|------|------|---------|---------|
| **基础服务** | KV, Cache | 无需启用 | ❌ 不检查 |
| **扩展服务** | SQL Database, Object Storage, Queues | 需要 `enabled: true` | ✅ 检查平台支持 |

### 扩展服务示例

```json
{
  "services": {
    "database": {
      "enabled": true,  // ← 必需
      "binding": "DB"
    }
  }
}
```

编译时会检查目标平台是否支持 SQL Database，不支持则报错并提供替代方案。

---

## 编译器行为

### 基础服务

```bash
$ deforge build --vendor tencent

✓ Basic services (KV, Cache) are always available
✓ No platform compatibility check needed
```

### 扩展服务

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

## 迁移指南

### 旧配置（已废弃）

```json
{
  "services": {
    "kv": {
      "enabled": true,     // ← 不再需要
      "namespace": "MY_KV"
    },
    "cache": {
      "enabled": true      // ← 不再需要
    }
  }
}
```

### 新配置（推荐）

```json
{
  "services": {
    "kv": {
      "namespace": "MY_KV"  // 仅保留平台特定配置
    }
    // cache 无需任何配置
  }
}
```

---

## 参考

- [Edge Canon 基础规范](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md)
- [Edge Canon 平台支持矩阵](https://github.com/deislet/edge-canon/blob/main/PLATFORM_MATRIX.md)
- [deforge 平台能力系统](../crates/deforge-core/src/platform_capabilities.rs)
