# deis-store

KV, per-tenant SQL and content-addressed blobs, as three gRPC services in one
process on SQLite and the local disk. Every call is scoped by the
`x-deis-tenant` header and, once an `auth_secret` is set, by a capability
token minted for that tenant. The crate docs in `src/lib.rs` describe the
layout; `proto/storage.proto` and `proto/blob.proto` are the contracts.

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
