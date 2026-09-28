# deis-store

KV, per-tenant SQL and content-addressed blobs, as three gRPC services in one
process on SQLite and the local disk. Every call is scoped by the
`x-deis-tenant` header and proved by an Ed25519 capability the control plane
signed for that tenant; the listener speaks TLS only. The crate docs in `src/lib.rs` describe the
layout; `proto/storage.proto` and `proto/blob.proto` are the contracts.

When `migration_admin_token` is configured, the process also exposes the
shared `deis.backend.migration.v1.BackendMigration` management service. It
accepts only `Authorization: Bearer <token>` and deliberately does not accept
the application data credential. Acquiring a tenant fence drains earlier KV,
SQL and blob writers, persists the fence in `migration-fences.sqlite`, and
refuses later writes until the exact pre-cutover release credential is used.
Inventory remains readable under the fence. An empty value fails configuration
validation; leaving the setting absent does not register the service at all.

## What one tenant can do to every other tenant

**There is no per-tenant quota.** A single object is capped
(`max_object_bytes`), a KV value and a SQL statement have their own ceilings,
and a listing is always paged. Nothing bounds how many objects, rows or bytes
one tenant may hold in total, and every tenant shares `blob.sqlite`, the
KV file and the one data directory. One tenant that keeps writing fills the
disk for all of them, and a full disk refuses every tenant's writes - the
blob store then also has to clean up after each refused upload, which it
does, but that is damage limitation rather than prevention. This is a product
decision not yet taken, not a missing check: a quota needs an owner for the
number (the control plane knows the tenants, this service does not) and a
story for what happens at the limit. Until then the operator's protection is
disk monitoring and the per-request ceilings above.

## Durability

Every SQLite file here runs in WAL mode with `synchronous = NORMAL` by
default; the `durability` setting chooses between `"normal"` and `"full"`.

Under `normal`, a commit's WAL frame is not fsynced until a checkpoint, so
a host that loses power may lose the last seconds of writes it had already
acknowledged: KV entries, blob metadata, and a tenant's own SQL. The
database cannot be corrupted by it - at recovery an incomplete WAL frame is
discarded whole, never half-applied. `full` closes the window at the cost of
an fsync per commit.

One asymmetry worth knowing before choosing: a blob's content file is put in
place before its metadata row commits, so a power failure under `normal` can
leave content with no row - an orphan, removable with the one-start `remove_orphaned_blob_content`
switch - but never a row whose content is missing.

The component-by-component tradeoff, with the queue and the control plane's
opposite choices, is ADR-012's 「各组件 SQLite 耐久性取舍」 table; the
queue's measured cost of `full` is in `crates/deis-queue/README.md`.
