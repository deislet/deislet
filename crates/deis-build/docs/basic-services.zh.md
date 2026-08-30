# 基础服务 (Basic Services)

> Edge Canon 规范要求所有平台必须支持的基础服务

---

## 概述

根据 [Edge Canon 基础规范](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md)，所有兼容平台必须提供以下基础服务：

1. **KV Storage** - 键值存储
2. **Cache API** - HTTP 响应缓存

这些服务**无需在配置中启用**，deis-build 编译器不会对它们进行平台兼容性检查，因为所有平台都保证支持。

---

## KV Storage

### 使用方式

`kv.put` 存的是字节：传一个字符串会按 UTF-8 编码，传 `ArrayBuffer` 或
`ArrayBufferView`（比如 `Uint8Array`）则原样落盘，图片、压缩帧这类非文本内容
不用先转成字符串再存。

```typescript
export default async function handler(context: Context) {
  // 直接使用，无需配置
  const kv = context.services.kv;

  await kv.put('user:123', JSON.stringify({ name: 'Alice' }));
  const data = await kv.get('user:123'); // 默认按文本读回

  return new Response(data);
}
```

`kv.get` 第二个参数的 `type` 决定怎么把存的字节读回来：`"text"`（默认，按
UTF-8 解码）、`"json"`（解码后再 `JSON.parse`）或 `"arrayBuffer"`（原始字节，
不经过任何解码，适合读回非文本内容）。这三个名字和别的平台一致；只有
`"stream"` 不支持——这个平台的读取是把整段值一次性取回来，没有流式读取可言，
传 `"stream"` 会直接抛出说明原因的错误，而不是悄悄退化成文本。

```typescript
const image = await kv.get('avatar:123', { type: 'arrayBuffer' });
const parsed = await kv.get('user:123', { type: 'json' }); // 等价于 JSON.parse(await kv.get(...))
```

`kv.put` 还接受 `expirationTtl`（秒数，到期后自动失效）与 `metadata`
（跟值一起存、`kv.getWithMetadata()` 单独读回，不占用值本身的大小）。

### 平台实现

| 平台 | 实现方式 | 值大小限制 |
|------|---------|-----------|
| Cloudflare | KV Namespaces | ≤ 25MB |
| Deno | Deno KV | ≤ 64KB |
| Tencent | EdgeKV | ≤ 2MB |
| Deislet | Runtime KV | ≤ 10MB |

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

启用之后，`context.services.database` 除了逐条 `execute`/`query`，还有两种
把多条语句合成一次的办法：

```typescript
// batch：所有语句提前定好，一次往返，要么全部生效要么全部不生效；
// 不能中途读一个结果再决定下一条语句，也不接受会返回行的 SELECT。
await database.batch([
  { sql: "UPDATE items SET stock = stock - 1 WHERE id = ? AND stock > 0", params: [id] },
  { sql: "INSERT INTO orders (item, buyer) VALUES (?, ?)", params: [id, buyer] },
]);

// transaction：回调里可以先查、按查到的结果再写，读写共享同一个连接，
// 直到回调结束才提交；连接是稀缺资源，所以这个连接会有一个超时时限。
const total = await database.transaction(async (tx) => {
  const [row] = await tx.query("SELECT balance FROM accounts WHERE id = ?", [id]);
  await tx.execute("UPDATE accounts SET balance = ? WHERE id = ?", [row.balance - 10, id]);
  return row.balance - 10;
});
```

拿不准选哪个：语句在开始前就能定好，用 `batch`；要先读后决定写什么，才用
`transaction`。

`context.services.queue` 同理：`send()` 现在存字节而不只是文本——字符串按
UTF-8 编码，对象先 `JSON.stringify` 再编码，`ArrayBuffer`/`ArrayBufferView`
原样发送。消费函数那一端，`message.body` 是发送时的原文字符串，但如果发的是
一段不是合法 UTF-8 的字节，`message.body` 会是 `null`（不是空字符串，也不是
乱码）；这种消息只能用 `message.bytes()` 读，`message.json()` 遇到 `body`
为 `null` 会直接抛出说明原因的错误，而不是试图解析一个不存在的文本。

---

## 编译器行为

### 基础服务

```bash
$ deis-build build --vendor tencent

✓ Basic services (KV, Cache) are always available
✓ No platform compatibility check needed
```

### 扩展服务

```bash
$ deis-build build --vendor tencent

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
- [deis-build 平台能力系统](../../deis-build-core/src/platform_capabilities.rs)
