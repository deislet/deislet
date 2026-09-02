# deis-telemetry

Logs, one trace per invocation, and metric samples, as three gRPC services in
one process on one SQLite file, plus a Prometheus scrape endpoint. Logs and
traces live in one table per UTC day so retention is a `DROP TABLE`; queries
page newest first by cursor. The crate docs in `src/lib.rs` are the full
description; `proto/telemetry.proto` is the contract.

## What one caller can do to every other caller

Two limits are absent on purpose, as product decisions that have not been
taken, not as oversights. Both are worth knowing before this service is put
in front of tenants that do not trust each other.

**There is no per-application quota.** Every string a producer sends is
capped - the message at 64 KiB, fields at 64 KiB, identifiers at 1 KiB - and
a stamp outside the retention window is refused, so one record cannot be
made arbitrarily large and one batch cannot create hundreds of partitions.
But nothing bounds how many records, series or bytes one `app_id` may
accumulate, and every application shares `telemetry.sqlite` and one data
directory. An application that logs in a tight loop fills the disk for every
application on this node, and when the disk is full every push from every
producer is refused until retention or an operator frees space. The
`/metrics` counters and the `grpc.health.v1` status say that it happened;
nothing stops it.

**`app_id` is not bound to the producer.** The capability token names this
service, not a tenant (see `src/auth.rs` for why it cannot be otherwise
today), so anything that holds the shared secret - every node - may push
records under any `app_id` and query any application's logs and traces. The
token keeps the network out; it does not keep one node's applications apart
from another's. A node that is compromised, or simply misconfigured, can
write lines into an application it does not run and read lines from one it
never saw.
