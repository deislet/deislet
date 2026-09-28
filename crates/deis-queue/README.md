# deis-queue

A durable queue with lease-based consumption: `Send` and `SendBatch`, `Pull`
under a lease, `Ack`, `Nack` with a delay, and a dead-letter table with the
reason a message was given up on. Nodes poll it; it never pushes. Every call
is scoped by the `x-deis-tenant` header and proved by an Ed25519 capability
the control plane signed for that tenant; the listener speaks TLS only. The crate docs in `src/lib.rs` describe
the design; `proto/queue.proto` is the contract.

The data directory also holds `backend-incarnation`, the UUIDv7 that names
this physical copy of the queue. It is minted the first time the directory is
opened and never copied into a snapshot, so a restore always starts a new one.

Consistent operator backups use `deis-queue snapshot --out <directory>` and
`deis-queue restore --from <directory>`. Snapshotting uses SQLite's
`VACUUM INTO`, records row counts and a SHA-256 in `snapshot.json`, and is safe
while the queue is live; copying a WAL-mode data directory with `cp -r` is not.
Restore requires an empty target, rechecks SQLite integrity and the manifest,
and creates a new backend incarnation instead of copying migration fences.

## Durability and its cost

The queue pins `synchronous = FULL` and WAL on open: an acknowledged `Send`
has to survive a host power loss, and that is where the at-least-once
promise rests. An `Ack` is cheaper in consequence - losing one only
redelivers the message, which is inside the same contract - but it commits
through the same setting. What the pin costs in throughput was measured, not
estimated, by two ignored tests, and the numbers of both runs are recorded
here; rerun them after a platform change and refresh this section rather
than trust old numbers.

`tests/durability_bench.rs` measures the commit rate directly: a
single-message `Send` (one `INSERT` in an `IMMEDIATE` transaction) and an
`Ack` (one `UPDATE` in its own) against a byte-for-byte copy of the queue's
real `messages` table and index, with a 1 KiB body - exactly the
one-commit-per-operation shape the service runs. Run it with
`cargo nextest run -p deis-queue -- --ignored`; it prints to stderr, and on
a host whose `/tmp` is tmpfs point `TMPDIR` at a real disk first, or every
fsync is a no-op and both columns read fast for the wrong reason. On the
machine that produced the numbers below - a 28-vCPU x86_64 VM (Intel Xeon
E5-2620 v4 @ 2.10 GHz), database on ext4 over `/dev/sda1`, debug build,
2026-09-25, median of four runs, 2,000 measured operations each after a
200-operation warm-up, single writer:

| setting | send (ops/s) | ack (ops/s) |
|---|---|---|
| `WAL` + `synchronous = NORMAL` | ~5.9k | ~10.2k |
| `WAL` + `synchronous = FULL` (what runs) | ~760 | ~640 |

On that disk a commit's fsync dominates, so `FULL` costs roughly an 8x
factor on send. That is the upper bound of the price, on slow storage.

The other measurement - `queue_throughput` in `src/queue.rs` - runs the
service itself: one producer and one consumer, 2,000 messages of 256 bytes
per round, 9 rounds, and the two `synchronous` modes alternate within each
round on a pool of one connection, so whatever else the host is doing lands
on both modes and the headline is the paired FULL/NORMAL ratio within a
round rather than two moments in time. It is a measurement, not an
assertion, so it prints the rates and passes; run it with

    cargo test -p deis-queue --lib -- --ignored queue_throughput --nocapture

and read the per-mode medians off stdout (nextest hides the stdout of a
passing test, so read the log it wrote instead). **Before running it, point
`TMPDIR` at a real disk.** Its first recorded run used the default temp
dir, which on this host is tmpfs, where an fsync is a no-op: it read NORMAL
4,019/s and FULL 3,984/s for send - the modes indistinguishable - which is
the in-memory speed limit, not the cost of the pin, and the trap this
measurement exists to catch. Rerun the same rounds with `TMPDIR` on the
ext4 disk, 2026-09-25, debug build:

| mode | send, median | ack, median | paired FULL/NORMAL ratio (send) |
|---|---|---|---|
| `NORMAL` | 3,321/s | 15,935/s | - |
| `FULL` (what runs) | 649/s | 10,919/s | 0.13–0.23 per round, ~5x |

The ack column moves far less than the send column: a `pull` of up to 100
messages is one transaction and the acks ride back in batches, so the extra
fsync per commit is paid roughly once per 100 acknowledgements, not once
per message. The send column is the one an operator plans against: on this
VM's virtio disk it is a ~5-8x factor (the direct commit bench above saw
8x, the service path 5x - same effect, different row shape), and on
NVMe-class storage the absolute rates rise while the ratio stays. One
escape valve either way: a `SendBatch` of *n* messages pays one commit,
not *n*, which is the path for producers that outrun the per-message rate.
The queue and the control plane pay this price deliberately, and the store
does not by default; the component-by-component reasoning is ADR-012's
「各组件 SQLite 耐久性取舍」 table.

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
