# deis-telemetry

Logs, one trace per invocation, and metric samples, as three gRPC services in
one process on one SQLite file, plus a Prometheus scrape endpoint. Logs and
traces live in one table per UTC day so retention is a `DROP TABLE`; queries
page newest first by cursor. The crate docs in `src/lib.rs` are the full
description; `proto/telemetry.proto` is the contract.

## Backup and restore

`deis-telemetry snapshot --out DIR` writes a consistent `telemetry.sqlite`
with `VACUUM INTO`, then records its SHA-256, byte size, logs/metrics/traces
counts and source backend incarnation in `snapshot.json`. Do not copy the
live WAL-mode database with `cp`.

`deis-telemetry restore --from DIR` accepts only an absent or empty target
(`--into DIR`, or the configured `data_dir`). It verifies SQLite integrity,
digest and counts before and after copying. The `backend-incarnation` record
is not part of a snapshot, so a restored data directory receives a fresh
backend incarnation.

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

**What a caller may touch is its capability's.** Every call carries a
capability deis-control signed (audience `telemetry`), verified against the
control plane's JWKS. A node's names `ingest` for each application
environment it runs plus `@platform` for its own records, so it cannot write
into an application it does not run and cannot read at all; a person's names
`read` for the environments their scopes reach, and queries and follow
streams return only those. `@platform` with `read` sees everything and is
the only capability the Prometheus page accepts. See `src/auth.rs`.
