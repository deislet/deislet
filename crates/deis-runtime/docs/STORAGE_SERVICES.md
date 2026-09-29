# Storage Services

How the logical resources exposed through `context.env.<NAME>` reach Deislet's
runtime services: which op talks to which backend, what crosses the Rust/JS
boundary as bytes versus text, and what happens when a resource was never
wired up for this deployment. Declared bindings are the only way in: there is
no `context.services` or `context.bindings`, and no unnamed namespace beside
the declared bindings.

| Service | Backend | Where |
|---|---|---|
| **Cache** | In-process (moka), one store per isolate; a named cache is a namespace inside it | `src/runtime/cache_ops.rs` |
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
manifest declares its bindings; `runtime/bindings.rs` builds a client for each
declared binding whose service the control plane gave an address for, and puts
it in the isolate's `OpState`. A storage binding without a client is not
published on `env` at all. An op whose kind has no client — because no binding
of that kind was declared, or the control plane sent no address for it —
returns its unavailable message immediately:

```
"{service} service is not available to this app: its manifest declares no
 {service} binding, or the control plane sent no address for it"
```

(`ops.rs::unavailable_message`, used by the KV, database and blob ops. The
queue writes its own in `queue_ops.rs::queue_unavailable`, because it has one
more thing to say: the message the application just handed over was not
enqueued anywhere and will not be delivered later.) That is the whole of it:
there has never been an in-memory `HashMap` or similar standing in for a
missing client. An application that sees this error has a configuration
problem — an undeclared binding or a control plane that was not told the
storage addresses — not a degraded mode to work around.

A binding name that is not among the declared ones of a kind that does have
clients is a different mistake and gets a different message from
`ops.rs::bound_client`:

```
"this app has no {service} binding named \"{name}\"; the {service} bindings
 its manifest declares are: {declared}"
```

There is deliberately no fallback to another namespace. An application that
asked for `HOT` and silently got a different one would read and write the
wrong namespace with nothing failing.

---

## Bytes across the boundary, not text

A value stored through a declared KV or Blob binding, or through the Cache API,
is whatever the application wrote — an image, a compressed record, a protobuf
— and only the application knows whether that happens to be text. So the op
boundary carries bytes, not a `String`:

```rust
// KV: reading crosses as a Uint8Array, not text. js/runtime.js decodes it as
// "text", "json" or leaves it as an ArrayBuffer, whichever kv.get(key,
// { type }) asked for.
async fn op_kv_get(...) -> Result<Option<ToJsBuffer>, JsErrorBox>

// Writing takes whatever ArrayBufferView the application handed to put().
async fn op_kv_set(..., #[buffer] value: JsBuffer, ...) -> Result<(), JsErrorBox>
```

Blob (`op_blob_get` / `op_blob_put`) and the Cache API (`op_cache_get` /
`op_cache_set`) take their bytes the same way, `#[buffer] JsBuffer` in. They
answer differently, because they have metadata to carry back beside the body:
`op_blob_get` resolves to `Option<BlobReadResult>` and `op_cache_get` to
`Option<CachedResponse>`, each a struct holding `body: ToJsBuffer` next to its
metadata, and `op_blob_put` answers `Option<BlobMetaResult>` with no body at
all. `op_kv_get` is the one that returns a bare `Option<ToJsBuffer>`, because
a KV value is only its bytes. What is identical is the part that matters: the
body is a buffer on both sides in every one of them.

This is deliberate and asymmetric — see `blob_ops.rs` for why `#[serde] Vec<u8>` is the wrong type on both sides
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

`context.env.<KV_BINDING>` → `KvClient` → deis-store's `KvStore` service
(`crates/deis-store/proto/storage.proto`): `Get`, `Set`, `Delete`, `List` and
`Has`. `Has` is reached by `KvClient::has` but has no JavaScript spelling; an
application asks by reading the key.

```javascript
export default async function handler(context) {
  const kv = context.env.SESSIONS;
  await kv.put("counter", "42");
  const value = await kv.get("counter"); // "42"
  const withMeta = await kv.getWithMetadata("counter");
  // => { value, metadata, expiration } | null
  const page = await kv.list({ prefix: "user:" });
  // => { keys: [{ name, expiration, metadata }, ...], list_complete, cursor }
  await kv.delete("counter");
}
```

`put(key, value, { expirationTtl, metadata })` accepts a string, an
`ArrayBuffer` or any `ArrayBufferView`; `get(key, { type })` decodes the
returned bytes as `"text"` (default), `"json"` or hands back an
`ArrayBuffer`. `metadata` is an opaque string deis-store stores and returns
untouched, read back through `getWithMetadata`.

`list` answers **one page**, not the whole key space, and not an array of
names. `keys` carries a record per key — `name`, `expiration` (the absolute
unix second it lapses at, `null` when it never does) and the decoded
`metadata` — and never the stored value, because a page may hold thousands of
keys. Page until `list_complete`, not until a page comes back short: a scan
that filled its page exactly gives a full page and then an empty one, so a
loop that stops on a short page is right only by luck.

```javascript
const all = [];
let page = { list_complete: false, cursor: undefined };
while (!page.list_complete) {
  page = await kv.list({ prefix: "session:", limit: 100, cursor: page.cursor });
  all.push(...page.keys);
}
```

`limit` is a request, not a guarantee: deis-store substitutes its own page
size (1000) when none is given and caps anything above its ceiling (10000).

## Database

`context.env.<DATABASE_BINDING>` → `DbClient` → deis-store's `Database`
service:
`Execute`, `Query`, `Batch`, `Prepare` / `ExecutePrepared` / `QueryPrepared`,
and the session-scoped `BeginTransaction` / `ExecuteInTransaction` /
`QueryInTransaction` / `CommitTransaction` / `RollbackTransaction` that back
`database.transaction(...)`.

```javascript
const database = context.env.PRIMARY_DB;
const rows = await database.query(
  "SELECT * FROM users WHERE id = ?", [123],
);
// => [ { id: 123, name: "Alice" }, ... ]  -- the rows themselves, an array

const exec = await database.execute(
  "INSERT INTO users (name) VALUES (?)", ["Alice"],
);
// => { rows_affected: 1, last_insert_id: 456 }

await database.transaction(async (tx) => {
  const [row] = await tx.query("SELECT balance FROM accounts WHERE id = ?", [id]);
  await tx.execute("UPDATE accounts SET balance = ? WHERE id = ?", [row.balance - 10, id]);
});
```

Read the two comments above literally, because the two calls do not answer
the same way:

  * `query` resolves to the **rows**, an array. `NativeDB.query` returns
    `res.rows` and nothing else. The column names do exist on the wire —
    `db_client.rs`'s `query` hands back `(rows, columns)` — but the
    JavaScript surface drops them, so there is no `.rows` or `.columns` to
    reach for. Destructuring works directly: `const [row] = await
    db.query(...)`.
  * `execute` resolves to an object with **snake_case** keys,
    `{ rows_affected, last_insert_id }`, built by `db_ops.rs::op_db_execute`.
    They are not `rowsAffected` / `lastInsertId`. And `last_insert_id` is
    *absent* rather than null when the statement inserted nothing, so test it
    with `"last_insert_id" in exec` or read it as possibly `undefined`.
    `batch` answers an array of exactly these objects, one per statement, in
    order.

One SQLite file per tenant on deis-store's side; `sql` and `params` cross as
text and JSON respectively, because a query and its parameter list are
exactly the string and the JSON values the application wrote, not an opaque
payload. `transaction(...)` pins one connection for the tenant for the life
of the callback (server-side deadline, default 5 s from
`DEFAULT_TRANSACTION_DEADLINE`, and `{ timeoutMs }` may only shorten it —
`TransactionLimits::grant` takes the smaller of the two); the outer logical
database binding is a different connection and does not see the transaction's
uncommitted writes. `tx.query` and `tx.execute` answer
exactly as their outer counterparts do. See `js/runtime.js`'s
`NativeTransaction` and section 6 of the repository's
`docs/architecture.zh.md`.

## Blob

`context.env.<BLOB_BINDING>` → `BlobClient` → deis-store's `BlobStore` service
(`crates/deis-store/proto/blob.proto`): `Get`, `Head`, `List`, `Put`,
`Delete`. Content-addressed on disk, with `Put` and `Get` streamed in chunks
so an object never has to sit in memory whole on either side.

```javascript
const blob = context.env.MEDIA;
const written = await blob.put("avatar", arrayBuffer);
// => { key, etag, size, contentType }
const object = await blob.get("avatar"); // null if absent
const bytes = await object.arrayBuffer();
const meta = await blob.head("avatar");  // no body
const page = await blob.list({ prefix: "uploads/" });
// => { objects: [{ key, etag, size, contentType, uploadedAt }, ...],
//      prefixes, list_complete, cursor }
await blob.delete("avatar");
```

`put` takes a string, an `ArrayBuffer` or any `ArrayBufferView` and answers
`{ key, etag, size, contentType }`.

`get` answers `null` for a key that was never written or has been deleted,
and otherwise an object carrying the bytes behind `bytes()`,
`arrayBuffer()`, `text()` and `json()`, plus `key`, `etag`, `size`,
`contentType` and `uploadedAt` — and `range` for a ranged read.
`get(key, { range })` takes either an HTTP `Range` header string, which is
what an application forwards when it was handed one, or the same range as
numbers (`{ offset, length }`, or `{ suffix }` for the last n bytes). A range
naming no byte of the object throws rather than answering `null`: "this
object has no such bytes" is a 416 and "there is no such object" is a 404,
and an application answering HTTP owes its caller different answers for the
two.

`list` pages the same way `kv.list` does — page until `list_complete`, not
until a page comes back short — with deis-store's own page size (200) and
ceiling (1000). A `delimiter` rolls folder names up into `prefixes`.

Writes may be conditional. `put(key, data, { onlyIf })` and
`delete(key, { onlyIf })` take HTTP's two spellings, `{ ifNoneMatch: "*" }`
for create-once and `{ ifMatch: etag }` for compare-and-set. **A conditional
write that lost resolves to `null`, and a conditional delete that lost
resolves to `false`** — nothing else in this API reports failure by its return
value. That is the one failure worth retrying by reading the object again;
every other failure throws, and must not be retried as it stands.

## Cache

`globalThis.caches.default` names the standard default cache.
`caches.open(name)` gives an application caches of its own beside it: there is
one store per isolate and a name is a namespace inside it, composed by
`cache_ops.rs::namespaced`, so opening a name costs nothing and two names
never see each other's entries. `caches.delete(name)` empties one and reports
whether it held anything. `caches.scope` publishes the paragraph below as a
value, so an application can check how far its cache reaches instead of having
to take this document's word for it.

Keys accept a string, a `Request` or a `URL`. `Cache-Control: max-age` / `s-maxage`
(`s-maxage` wins when both are present) decides how long an entry stays
fresh; a response with no such header gets the node's default; `no-store` or
`private` is refused with a `TypeError` rather than cached. `delete()`
reports whether there was anything to delete.

Scope is one isolate: a later request to the same `(application,
environment)` on the same node sees a hit, a different node or a new isolate
(a fresh deployment) does not. It is an optimisation, not a place to keep
state.

## Queue

`context.env.<QUEUE_BINDING>` → `QueueClient` → deis-queue's `Queue` service
(`crates/deis-queue/proto/queue.proto`: `Send`, `SendBatch`, `Pull`, `Ack`,
`Nack`, `ListDeadLetters`, `ReplayDeadLetters`). An application has `send()`,
`sendBatch()`, and `to(name)` for a queue other than the default.
`sendBatch(messages)` enqueues a whole array as one unit — deis-queue stores
it in one transaction, so a rejected batch has enqueued nothing at all, and
it costs one op dispatch and one round trip rather than one per message. Its
entries must be `{ body }` objects, deliberately not the bare values `send`
takes: `send({ body: x })` already means "JSON-encode this object", so
accepting both shapes in one call would make code ported from Cloudflare
enqueue the literal text `{"body":"..."}` with nothing reporting it.

Consumption is the node's own poller (`trigger/`), not something an
application calls directly — it leases a batch, hands it to the application's `queue` export,
and the export's `message.ack()` / `message.retry()` decide what happens
next. A message body crosses as bytes end to end, same as KV and blob: a
queue carries whatever the application put on it, and the dead-letter view is
typed as bytes for the same reason.

`send()` encodes a string as UTF-8 and an `ArrayBuffer`/`ArrayBufferView` as
its own bytes unchanged; anything else — an object, an array, a number — is
JSON-encoded and then UTF-8 encoded.

On the receiving side `message.body` is the text as it was sent — but only
when the body decodes as UTF-8; a body that does not (a buffer sent as its own bytes need
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
