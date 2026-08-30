# Storage Services

What `context.services.*` actually does on the runtime side: which op talks to
which backend, what crosses the Rust/JS boundary as bytes versus text, and
what happens when a service was never wired up for this deployment.

| Service | Backend | Where |
|---|---|---|
| **Cache** | In-process (moka), one cache per isolate | `src/runtime/cache_ops.rs` |
| **KV** | deis-store's `KvStore`, over gRPC | `src/runtime/kv_client.rs`, `src/runtime/ops.rs` |
| **Database** | deis-store's `Database`, one SQLite file per tenant, over gRPC | `src/runtime/db_client.rs`, `src/runtime/db_ops.rs` |
| **Blob** | deis-store's `BlobStore`, content-addressed, streamed, over gRPC | `src/runtime/blob_client.rs`, `src/runtime/blob_ops.rs` |
| **Queue** | deis-queue's `Queue`, lease-based, over gRPC | `src/runtime/queue_client.rs`, `src/runtime/queue_ops.rs` |

All four remote services are one process's worth of work
([`deis-store`](../../deis-store) for the first three, [`deis-queue`](../../deis-queue)
for the fourth) reached from this node's own gRPC clients. There is no
"deis-control server" on this path — the control plane's only involvement is
handing the node the two addresses (`store_endpoint`, `queue_endpoint`) and,
when a shared secret is configured, minting the per-tenant capability token
that authorises this node's calls (see `runtime/bindings.rs` and
`crates/deis-store/src/auth.rs`).

---

## No fallback: an unwired service is a hard error, not a stand-in

None of the four ever falls back to an in-memory store. A deployment's
manifest declares which services it uses; `runtime/bindings.rs` builds a
client for each declared service that the control plane gave an address for,
and puts it in the isolate's `OpState`. An op whose client is missing —
because the manifest never declared the service, or the control plane sent no
address for it — returns `service_unavailable(name)` immediately:

```
"{service} service is not available to this app: its manifest does not
 declare \"{service}\", or the control plane sent no address for it"
```

(`ops.rs::service_unavailable`, used identically by the KV, database, blob and
queue ops.) That is the whole of it: there has never been an in-memory
`HashMap` or similar standing in for a missing client. An application that
sees this error has a configuration problem — an undeclared service or a
control plane that was not told the storage addresses — not a degraded mode
to work around.

---

## Bytes across the boundary, not text

A value stored through `context.services.kv`, `context.services.blob` or
`context.services.cache` is whatever the application wrote — an image, a
compressed record, a protobuf — and only the application knows whether that
happens to be text. So the op boundary carries bytes, not a `String`:

```rust
// KV: reading crosses as a Uint8Array, not text. js/runtime.js decodes it as
// "text", "json" or leaves it as an ArrayBuffer, whichever kv.get(key,
// { type }) asked for.
async fn op_kv_get(...) -> Result<Option<ToJsBuffer>, JsErrorBox>

// Writing takes whatever ArrayBufferView the application handed to put().
async fn op_kv_set(..., #[buffer] value: JsBuffer, ...) -> Result<(), JsErrorBox>
```

Blob (`op_blob_get` / `op_blob_put`) and the Cache API (`op_cache_get` /
`op_cache_set`) follow the identical shape: `#[buffer] JsBuffer` in,
`ToJsBuffer` out. This is deliberate and asymmetric — see
`blob_ops.rs` for why `#[serde] Vec<u8>` is the wrong type on both sides
(`serde_v8` does not accept it as a typed array either direction) and
`cache_ops.rs`'s module comment for what carrying a Cache API body as a
string used to do: decode it as UTF-8 on write, which replaces every
ill-formed byte sequence with U+FFFD, so anything outside the ASCII range —
an image cached through `caches.default`, which every isolate has whether the
application asked for it or not — came back corrupted with nothing reporting
an error.

The one thing that stays text is metadata *about* the bytes: KV's optional
`metadata` string (`op_kv_get_with_metadata`), a database `sql` string and its
JSON-encoded `params`, and the Cache API's status/headers, which travel as one
JSON string alongside the body buffer.

The request and response bodies a handler sees follow the same rule, one
level up: `JsRequest.body` / `JsResponse.body` cross the boundary as
`Option<Vec<u8>>` (`ops.rs::PendingRequestBody` / `PendingResponseBody`), not
as a string — an upload or an upstream response that is not valid UTF-8
survives the trip intact.

---

## KV

`context.services.kv` → `KvClient` → deis-store's `KvStore` service
(`crates/deis-store/proto/storage.proto`): `Get`, `Set`, `Delete`, `List`.

```javascript
export default function handler(context) {
  await context.services.kv.put("counter", "42");
  const value = await context.services.kv.get("counter"); // "42"
  const withMeta = await context.services.kv.getWithMetadata("counter");
  // => { value, metadata, expiration } | null
  const keys = await context.services.kv.list("user:");    // string[]
  await context.services.kv.delete("counter");
}
```

`put(key, value, { expirationTtl, metadata })` accepts a string or any
`ArrayBufferView`; `get(key, { type })` decodes the returned bytes as `"text"`
(default), `"json"` or hands back an `ArrayBuffer`. `metadata` is an opaque
string deis-store stores and returns untouched, read back through
`getWithMetadata`.

## Database

`context.services.database` → `DbClient` → deis-store's `Database` service:
`Execute`, `Query`, `Batch`, `Prepare` / `ExecutePrepared` / `QueryPrepared`,
and the session-scoped `BeginTransaction` / `ExecuteInTransaction` /
`QueryInTransaction` / `CommitTransaction` / `RollbackTransaction` that back
`database.transaction(...)`.

```javascript
const result = await context.services.database.query(
  "SELECT * FROM users WHERE id = ?", [123],
);
// => { rows: [...], columns: [...] }

const exec = await context.services.database.execute(
  "INSERT INTO users (name) VALUES (?)", ["Alice"],
);
// => { rowsAffected: 1, lastInsertId: 456 }

await context.services.database.transaction(async (tx) => {
  const [row] = await tx.query("SELECT balance FROM accounts WHERE id = ?", [id]);
  await tx.execute("UPDATE accounts SET balance = ? WHERE id = ?", [row.balance - 10, id]);
});
```

One SQLite file per tenant on deis-store's side; `sql` and `params` cross as
text and JSON respectively, because a query and its parameter list are
exactly the string and the JSON values the application wrote, not an opaque
payload. `transaction(...)` pins one connection for the tenant for the life
of the callback (server-side deadline, default 5 s, `{ timeoutMs }` may only
shorten it); the outer `context.services.database` handle is a different
connection and does not see the transaction's uncommitted writes. See
`js/runtime.js`'s `NativeTransaction` and `docs/architecture.zh.md` section 6.

## Blob

`context.services.blob` → `BlobClient` → deis-store's `BlobStore` service,
content-addressed on disk, `Put`/`Get` streamed in chunks so an object never
has to sit in memory whole on either side.

```javascript
await context.services.blob.put("avatar", arrayBuffer);
const object = await context.services.blob.get("avatar"); // null if absent
const bytes = await object.arrayBuffer();
await context.services.blob.delete("avatar");
```

`put` takes a string, an `ArrayBuffer` or any `ArrayBufferView`; `get`
answers an object with `arrayBuffer()`, `text()` and `json()`, or `null` for
a key that was never written or has been deleted.

## Cache

`context.services.cache`, and `globalThis.caches.default` names the identical
instance (the Cloudflare-style global some code expects to find). Keys accept
a string, a `Request` or a `URL`. `Cache-Control: max-age` / `s-maxage`
(`s-maxage` wins when both are present) decides how long an entry stays
fresh; a response with no such header gets the node's default; `no-store` or
`private` is refused with a `TypeError` rather than cached. `delete()`
reports whether there was anything to delete.

Scope is one isolate: a later request to the same `(application,
environment)` on the same node sees a hit, a different node or a new isolate
(a fresh deployment) does not. It is an optimisation, not a place to keep
state.

## Queue

`context.services.queue` → `QueueClient` → deis-queue's `Queue` service:
`send()`, and `to(name)` for a queue other than the default. Consumption is
the node's own poller (`trigger/`), not something an application calls
directly — it leases a batch, hands it to the application's `queue` export,
and the export's `message.ack()` / `message.retry()` decide what happens
next. A message body crosses as bytes end to end, same as KV and blob: a
queue carries whatever the application put on it, and the dead-letter view is
typed as bytes for the same reason.

`send()` encodes a string as UTF-8, a plain object as JSON then UTF-8, and an
`ArrayBuffer`/`ArrayBufferView` as its own bytes unchanged. On the receiving
side `message.body` is the text as it was sent — but only when the body
decodes as UTF-8; a body that does not (a buffer sent as its own bytes need
not be text at all) leaves `message.body` as `null` rather than a decoding
error or a lossily-decoded string. `message.bytes()` reads the bytes
regardless of whether they are text; `message.json()` throws, naming the
message, when `body` is `null` rather than parsing text that was never
there.

---

## Tests

- `cache_ops_test.rs` — Cache API round trips, including
  `a_cached_body_that_is_not_text_survives_the_round_trip`.
- `kv_client.rs`'s own tests — KV round trips against a real deis-store,
  including `a_value_that_is_not_valid_utf8_survives_the_round_trip` and the
  capability-token checks.
- `db_client.rs`'s own tests and `db_ops_test.rs` — `execute` / `query` /
  `batch` / prepared statements / `transaction`, against a real deis-store.
- `blob_client_test.rs` — blob round trips against a real deis-store,
  including `multi_chunk_object_round_trips_byte_for_byte`.
- `queue_consumer_test.rs` — the poller: a batch handled, failed, retried or
  handed back after its lease lapsed, against a fake queue client; the wire
  round trip itself is `crates/deis-queue`'s own `grpc_e2e.rs`.

The KV, database and blob tests are not `#[ignore]`d waiting on a separate
server process: each starts its own instance of deis-store in-process for the
duration of the test and talks to it over a real socket. The queue poller's
own tests run against a fake queue client instead, because what they are
proving is the poller's retry and lease-handling logic, not the wire; the
wire round trip is deis-queue's own `grpc_e2e.rs`, likewise a real service
started in-process. `scripts/demo.sh verify` additionally exercises KV and
the queue end to end through a deployed application, against the real
processes the demo starts.

## License

Apache-2.0
