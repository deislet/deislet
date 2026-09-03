# 基础服务 (Basic Services)

> 选定 Edge Canon 标准版本要求的基础服务

---

## 概述

根据 [Edge Canon 基础规范](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md)，所有兼容平台必须提供以下基础服务：

1. **KV Storage** - 键值存储
2. **Cache API** - HTTP 响应缓存

这些服务**无需在配置中启用**，走共同构建路径。后端是否符合它们的标准语义，只能由
Edge Canon 一致性测试和 registry 证明，编译器不能根据目标名称推断。

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

### 标准与目标状态

选定的 Edge Canon release 定义可移植 KV 行为和最低限额。在本指南复制供应商产品名或
value-size 数值，不能证明后端实现了这些语义。当前后端证据、认证和支持状态只看
[registry 派生视图](../../../docs/product/target-status.zh.md)。

### 配置

```json
{
  "services": {
    "kv": { "enabled": true }
  }
}
```

`enabled` 是唯一的字段。这里曾经还有 `namespace` 和 `connectionString`，两个都在 2026-08-30 删掉了：**全仓没有一个组件读过它们**。构建产出的 bundle manifest 把服务记成布尔值，而一个租户已经由 `(应用, 环境)` 唯一确定，所以一个应用的 KV 天然就是它自己的，没有第二个东西需要起名字来区分。

一个应用想要两个逻辑上分开的存储，就给键加前缀，然后用 `kv.list({ prefix })` 按前缀列举——前缀之间互不干扰。如果要的是**平台强制保证的边界**而不只是组织方式，那就是另一个环境或另一个应用，那才是 deis-store 每次调用都会校验的租户边界。

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

### 标准与目标状态

Cache 是选定标准的 mandatory 能力；这项要求不等于每个后端当前已经合规。编译器在这里
不按目标名称推断，只有共享测试套件和 registry 才能建立合规结论。

---

## 与扩展服务的区别

| 类型 | 示例 | 配置要求 | 构建检查 |
|------|------|---------|---------|
| **基础服务** | KV, Cache | 无需启用 | ❌ 不检查 |
| **扩展服务** | SQL Database, Object Storage, Queues | 需要 `enabled: true` | ✅ 检查当前目标打包器映射 |

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

编译时，Deislet 检查自己的当前目标打包器是否有 SQL Database 映射；缺少映射会终止
目标产物构建。这不是供应商能力、合规或支持结论；这些状态只看
[registry 派生视图](../../../docs/product/target-status.zh.md)。

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
# 不打印能力判断。KV、Cache 走共同产物路径；部署后的语义由一致性测试检查，
# 不是由这次构建证明。
```

### 扩展服务

```bash
$ deis-build build --vendor tencent

✗ Target artifact implementation check failed
Project cannot yet be compiled by Deislet's EdgeOne Pages target packager:
1. Feature 'SQL Database': no current target implementation
```

---

## 迁移指南

### 旧配置

```json
{
  "services": {
    "kv": {
      "enabled": true,
      "namespace": "MY_KV"      // ← 已删除，从来没有组件读过
    },
    "cache": {
      "enabled": true           // ← 不需要：cache 是基本能力，无需声明
    }
  }
}
```

### 现在

```json
{
  "services": {
    "kv": { "enabled": true }
  }
}
```

未知的键仍然会被接受而不是报错，所以带着旧字段的项目配置照样能解析——那两个字段只是不再有任何作用，而且此前也没有。

---

## 参考

- [锁定的 Edge Canon 标准与 registry](../../../standards/edge-canon.lock.json)
- [deis-build 目标产物覆盖](../../deis-build-core/src/target_artifact_coverage.rs)
