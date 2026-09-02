# deis-queue

A durable queue with lease-based consumption: `Send` and `SendBatch`, `Pull`
under a lease, `Ack`, `Nack` with a delay, and a dead-letter table with the
reason a message was given up on. Nodes poll it; it never pushes. Every call
is scoped by the `x-deis-tenant` header and, once an `auth_secret` is set, by
a capability token for that tenant. The crate docs in `src/lib.rs` describe
the design; `proto/queue.proto` is the contract.

## What one tenant can do to every other tenant

**There is no per-tenant quota.** One message body, one batch and one pull
each have a ceiling, and a pull response stops at half the frame size.
Nothing bounds how many messages one tenant may leave in a queue, or in how
many queues, and every tenant shares `queue.sqlite` and the one data
directory. A producer that sends faster than its consumer pulls grows the
file without limit, and when the disk is full every tenant's `Send` is
refused, not only the one that filled it. This is a product decision not yet
taken - a quota needs an owner for the number, which is the control plane's
view of tenants rather than this service's - so what an operator has today is
disk monitoring and the per-request ceilings above.
