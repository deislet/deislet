# Backing up and restoring the control plane

The control plane is one SQLite database with one process writing it, and
embedded installations run exactly one of it. This document is the operator's
side of that decision (D-4): what one backup has to contain, how it is taken
continuously, what the recovery procedure is, what it costs in data loss and
downtime, and how any of it is verified.

The decision itself, the RPO/RTO numbers, the availability promise while the
control plane is down, and the constraints everywhere else in the platform has
to respect are in [ADR-012](adr/012-pluggable-persistence.md) (embedded
control section) and [ADR-024](adr/024-coordinated-embedded-backend-restore.md)
(the coordinated restore of the three data-plane backends, which is a different
procedure for different data).

## Two kinds of data, two tools

An installation's state lives in two places, and each has its own backup tool.
They do not overlap and neither replaces the other:

| Data | Tool | Recovery point |
|---|---|---|
| The control plane: `control.db`, bundles, keys, machine identity | Litestream plus file copies (this document) | continuous, about one second behind |
| The data services: deis-store, deis-queue and deis-telemetry directories | `deis-backend-topology` ([ADR-024](adr/024-coordinated-embedded-backend-restore.md)) | the barrier time of the chosen snapshot set |

- **Litestream covers the control plane only.** It replicates one SQLite file
  written by one process. It knows nothing about the three data services, whose
  databases are many files per tenant, written by other processes.
- **`deis-backend-topology` covers the data services only.** It takes a
  snapshot of all three under one write barrier (so a queue message and the KV
  write it refers to come from the same moment) and restores all three
  together, with a journal that survives a crash half way through. It never
  touches `control.db`.
- Both assume a single installation: one control process, and one store, one
  queue and one telemetry service on the same host as the driver. A restore
  runs inside that one control process; there is no second control instance
  to hand it over to. If control stops half way through a restore, the next
  start picks the operation up again from its durable state and the driver's
  journal.

## The restore set

A restore that has the database and not the rest comes up healthy, answers
`/health` with `OK`, lists every application in the console, and cannot run one
of them. All of it belongs to one backup policy, and by default all of it is in
one directory:

| What | Where it is by default | How it is captured |
|---|---|---|
| The catalogue | `<data_dir>/control.db` | Litestream, continuously (this document) |
| Every version's code | `<data_dir>/bundles/` | content-addressed files; `rclone sync` or restic, every minute, verified by sha256 at restore |
| The secret master key | `<data_dir>/keys/master.key`, named by `secret_key_file` | copied when it changes or is rotated, into storage separate from the data |
| The issuer private key | `<data_dir>/keys/` | as the master key |
| The machine channel identity | `<data_dir>/machine/`: certificate, private key, fleet trust root | copied when it changes or is rotated, protected separately from the master key |

`<data_dir>` is `/var/lib/deis-control` unless the configuration says
otherwise, and `control.db` is `<data_dir>/control.db` unless `--database`
says otherwise. Both have to be absolute paths: a relative one names a
different file to every process started from a different directory, and the
binary refuses one at startup rather than discovering it later.

Two things are deliberately not in the set:

- **`<data_dir>/bootstrap.token`** is a way in, not state. It expires 24 hours
  after it was minted if it was never used, and a restored installation mints a
  fresh one by itself if it has no live credential.
- **The TLS certificate for the human listener**, when one is configured. It
  is issued by whatever CA the operator uses and can be re-issued; it is not
  state.

The list is not only documentation. Control prints it at startup (saying which
entries are *outside* `data_dir`, and which therefore need their own backup),
and `deis-control --check-config` prints every entry:

```console
$ deis-control -c /etc/deis/control.toml --check-config
configuration is usable: port 8080, machine channel 10.0.0.1:8444, human API authenticated ...
one backup has to cover all of this, together:
  control database: /var/lib/deis-control/control.db
  uploaded bundles: /var/lib/deis-control/bundles
  secret master key: /etc/deis/secret.key
  machine channel certificate: /etc/deislet/control.crt.pem
  machine channel private key: /etc/deislet/control.key.pem
  fleet trust root: /etc/deislet/fleet-ca.crt.pem
```

An installation that keeps the key material outside `data_dir` - which is a
reasonable thing to do, and what the example configuration and the canary
installer both do - gets a warning naming each such file on every start, and
has to include it in whatever backs up that directory.

The database path in that list is derived from `data_dir`, since that is the
only thing the configuration names; an installation whose unit passes
`--database` explicitly may keep the file under another name. The rest of the
list is what the configuration resolves to on its own.

## Continuous backup: Litestream

[Litestream](https://litestream.io) replicates the database's write-ahead log to
another disk or to S3-compatible object storage, one second at a time, and does
not need the control plane to be stopped to do it.

The samples in this repository:

- [`scripts/canary/litestream.yml`](../scripts/canary/litestream.yml) - the
  service's configuration, installed as `/etc/litestream.yml`.
- [`scripts/canary/systemd/litestream.service`](../scripts/canary/systemd/litestream.service) -
  the unit, installed beside the other Deislet units.

Both are written for the **0.5 series, pinned at 0.5.17**. The 0.5 and 0.3
series differ in the configuration format (`replica`, one object, replaced the
`replicas` array), in the replica format, and in the `-json` flag the restore
drill uses. Moving between them is a change to the backup, not an upgrade of a
tool: read <https://litestream.io/docs/migration/> first and treat it as a
release of its own.

```bash
# The binary, at the pinned version, verified against the release checksums.
curl -fsSLo /tmp/litestream.tar.gz \
  https://github.com/benbjohnson/litestream/releases/download/v0.5.17/litestream-0.5.17-linux-x86_64.tar.gz
curl -fsSL https://github.com/benbjohnson/litestream/releases/download/v0.5.17/checksums.txt \
  | grep 'litestream-0.5.17-linux-x86_64.tar.gz' > /tmp/checksums.txt
(cd /tmp && sha256sum -c checksums.txt)   # adjust the file name if the host is not x86_64
tar -xzf /tmp/litestream.tar.gz -C /tmp litestream
install -o root -g root -m 755 /tmp/litestream /usr/local/bin/litestream

# The configuration: set `path` to <data_dir>/control.db and `url` to the
# replica (s3://bucket/prefix or file:///mnt/backup/deis-control).
install -o root -g root -m 644 scripts/canary/litestream.yml /etc/litestream.yml
install -o root -g root -m 644 scripts/canary/systemd/litestream.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now litestream

litestream status          # replication lag and last sync per database
journalctl -u litestream -f
```

Credentials for S3-compatible storage belong in the unit's environment
(`LITESTREAM_ACCESS_KEY_ID`, `LITESTREAM_SECRET_ACCESS_KEY`) or in the unit's
`EnvironmentFile`, never in `/etc/litestream.yml`, which anyone who can read
`/etc` can read. Whatever the account is, it needs write access to the replica
prefix and nothing else.

The unit is ordered after `deis-control.service` and is *not* tied to it: the
control plane can be restarted - including both halves of a blue-green switch,
which share one database file - without the one Litestream process that may
ever replicate that file being restarted with it.

## RPO and RTO

| Term | Value | What it depends on |
|---|---|---|
| RPO, catalogue | about one second | `sync-interval: 1s` in the Litestream configuration. A commit is already durable in the local WAL; this is how long it may exist nowhere else |
| RPO, bundles | one minute | the `rclone sync` / restic job's own schedule |
| RPO, key material | zero | it is copied when it changes, so there is no window in which a change exists only locally |
| RTO, catalogue | 15 minutes | from "the control host is declared unavailable" to "the new instance passes readiness", by the runbook below. Single-instance is the accepted cost: while the control plane is down, nothing can be deployed, rolled back, rotated or created |

The drill measures the dominant term of that RTO - the restore, the start and
the read of the catalogue - and writes it, with the data's timestamp, into the
evidence file described below. **No evidence file is committed in this
repository yet**: the drill has not been run against a real replica here, so
the 15 minutes above is the target from ADR-012 and not a measured figure. The
first real drill is what makes it a measured one.

## Runbook: the control host is gone

Written for a person under pressure. The drill exists so this path has been
walked before; run it against a copy first if there is time.

1. **Declare the host unavailable, and make it so.** If the old host can come
   back, it must not: two control planes against one database replicate a
   database that is being written (see the constraints below) and can both
   answer nodes. Power it off, or isolate its network, or make sure both
   `deis-control` and `litestream` are stopped there.
2. **Restore the database** on the new host, into an empty directory:

   ```bash
   install -d -o deis -g deis -m 750 /var/lib/deis-control
   litestream restore -o /var/lib/deis-control/control.db s3://deis-control-backup/control
   ```

   `litestream restore` refuses to overwrite a non-empty file, which is the
   behaviour to want here: an existing database means someone has already
   started.
3. **Check what came back** before starting anything:

   ```bash
   sqlite3 /var/lib/deis-control/control.db 'PRAGMA integrity_check;'   # must be ok
   sqlite3 /var/lib/deis-control/control.db 'PRAGMA user_version;'      # the schema version
   ```

   Use the binary that is the same version as, or newer than, the one that took
   the backup: a restored database whose `user_version` is above what the
   binary knows is refused at startup rather than served with a schema it does
   not understand.
4. **Restore the key material and the bundles** from their own backups into
   `<data_dir>`, with the permissions they had (`keys/` and `machine/` are not
   world-readable; the private key files are `0600`), and verify the bundle
   sha256s as part of that restore.
5. **Check the configuration, then start:**

   ```bash
   deis-control -c /etc/deis/control.toml --check-config
   systemctl start deis-control
   curl -fsS http://127.0.0.1:8080/health      # OK
   ```

   `--check-config` prints the restore set: if one of those paths is missing or
   empty, this is the moment to notice.
6. **Read the catalogue**, with a credential, and compare the application list
   with what the installation is supposed to be serving. This is the check that
   the database which came back is the one that was meant: an empty catalogue
   and a healthy process look exactly alike from `/health`.
7. **Start Litestream again**, on the new host:

   ```bash
   systemctl start litestream
   litestream status
   ```

   It begins a new generation for the restored database. The old generation is
   still in the replica and is cleaned up by Litestream's retention policy
   (`snapshot.retention`, 168 hours in the sample); nothing needs to be deleted
   by hand.
8. **Tell the fleet.** Nodes re-register on their own when they can reach the
   machine channel; edges keep serving the routes they had. What is *not*
   restored is in-flight state: deployments that were mid-flight are taken back
   by the new instance's first start (the process records that it withdrew
   them), and anything that was only in the old instance's memory is gone.

## Constraints every operator has to respect

These are the ways a working backup is broken by something else touching the
database. They are Litestream's constraints; the platform's side of them is
already enforced (control contains no `wal_checkpoint` call at all, and a grep
test in `crates/deis-control` keeps it that way).

- **One Litestream per database.** Two processes replicating the same file
  produce a corrupt replica. Two *control* processes on one database file are
  fine and expected (a blue-green switch, a hand-started instance beside the
  service unit); two Litestreams are not.
- **Nothing else may truncate the WAL.** No `PRAGMA wal_checkpoint(TRUNCATE)`
  or `(RESTART)` from an operator, a script, or another tool. Control never
  issues one; Litestream checkpoints in PASSIVE mode and holds a read
  transaction so the WAL cannot be truncated before it has copied it.
- **Do not replace the database file while Litestream runs** - not with `cp`,
  not with `rsync`, not by restoring over it - and do not `VACUUM` it. `VACUUM
  INTO` (which writes a *new* file) is the exception and is what the
  pre-migration snapshot uses. Restoring means stopping Litestream and control,
  then restoring, then starting them again.
- **The database has to be on a local filesystem.** WAL mode requires it. NFS
  or SMB is not a supported home for it.
- **A replica is not a file copy.** `cp -r` or `rsync` of a live WAL database
  silently loses the commits still in the `-wal` file; that is why Litestream
  exists. If you must take a manual copy, stop the control plane first, or use
  `sqlite3 <db> ".backup <target>"` / `VACUUM INTO`.
- **Schema migrations are followed by a large WAL.** Migrations run in
  `BEGIN IMMEDIATE` transactions, which is compatible with Litestream, but the
  twelve-step rebuild that drops foreign keys writes a lot at once. After a
  migration, confirm Litestream has caught up (`litestream status`) before doing
  anything else.
- **Litestream is version-pinned and not auto-upgraded.** 0.5 and 0.3 do not
  read each other's replicas.

## While the control plane is down: what still works

The data plane does not depend on the control plane being online. This table is
the promise, and the mechanism behind each row is in
[ADR-025](adr/025-machine-identity-and-capabilities.md) (capabilities, offline
caches, certificate lifetime) and in the routing-table persistence described
below.

| During a control-plane outage | 0-24 hours | Beyond 24 hours |
|---|---|---|
| Requests to already-deployed apps | available | available (whatever does not need storage) |
| store/queue/telemetry access | available (grace) | interrupted |
| Restarting a proxy | available (persisted routing table) | available |
| Restarting a node | available (offline cache) | apps start; storage access interrupted |
| Deploy, roll back, rotate a secret, create an app | unavailable | unavailable |
| Revocation taking effect | only revocations issued before the outage | same |

Two prerequisites are worth stating plainly, because both used to be unstated:

- An edge keeps serving the last routing table it fetched, and it writes that
  table to local disk atomically with a generation number, so a proxy that
  *restarts* while the control plane is down still knows the routes. A route
  created during the outage reaches no edge until the control plane is back.
- A node that restarts during the outage starts the apps that do not reference
  a secret, and the ones that do from its encrypted offline cache. Nodes whose
  certificate expires during the outage are the hard limit: certificates are
  issued with a lifetime of at least 72 hours for that reason.

## The recovery drill

`scripts/control-restore-drill.sh` is the evidence. It restores a replica into
an isolated directory, checks it, starts a control plane against it on loopback
with a configuration that names no production node and no storage endpoint,
reads the catalogue through the API, and writes a JSON report into
`docs/product/evidence/`.

```bash
scripts/control-restore-drill.sh \
  --replica s3://deis-control-backup/control \
  --config /etc/deis/control.toml \
  --read-token "$(deis token create --account drill --scope 'read:*/*' drill-read)" \
  --source-database /var/lib/deis-control/control.db \
  --expect-user-version 3
```

What the report contains, and what each assertion means:

| Assertion | What it proves |
|---|---|
| `replica-restored` | `litestream restore` wrote a non-empty database |
| `integrity-check` | `PRAGMA integrity_check` answered `ok` |
| `replica-is-current` | the restored data is no further behind the source than `--max-source-lag-seconds` allows (default 300, against an RPO of one second) |
| `user-version-known` | with `--expect-user-version`: the restored schema is not newer than the binary |
| `configuration-usable` | `deis-control --check-config` accepted the configuration the restored installation runs |
| `control-answers` | the control plane started against the restored database answered `/health` |
| `read-only-api-matches-database` | the application list served from the restored database matches the table it was read from |

Pass `--read-token` (or set `DEIS_RESTORE_DRILL_TOKEN`), or the outcome is
`partial`: `/health` is a constant, and a drill that only calls it has shown
that a process starts and nothing about the catalogue. Pass
`--source-database` to check how current the replica is, which is the one thing
a rehearsal on a live installation can answer and a real recovery cannot.

The run writes `docs/product/evidence/control-restore-drill-<UTC>.json` and
validates it against
[`control-restore-drill.schema.json`](product/evidence/control-restore-drill.schema.json)
before it lands. Keep the file: only the most recent rehearsal counts as
evidence (ADR-012). Run it at least once a quarter, and after any change to the
backup configuration, the Litestream version, the schema migration set, or the
location of the key material.

Commit the JSON. Where an installation's evidence cannot be published, keep the
outcome and the durations in the operator's own record - but keep them, because
"the backup runs" is a claim about a job and this is a claim about a restore.

### What the drill does not cover

- **Bundle contents.** The drill counts the bundles beside the database; it does
  not fetch and verify them. That is the file-sync job's own restore exercise
  (restic `check` / `rclone check` against the sha256 in the version id).
- **A nightly run.** There is no scheduled job in this repository yet; the
  drill is run by hand and its evidence is committed by hand. A MinIO container
  plus a scheduled workflow is the obvious next step.
- **Governance registration.** The report is validated against its schema by
  the drill itself. Registering that schema with the repository's
  product-governance check (so a stale or malformed evidence file fails the
  build) is still to come.

## Where else this is documented

- [`crates/deis-control/DEPLOYMENT.md`](../crates/deis-control/DEPLOYMENT.md) -
  installing and running the control plane, including the manual backup
  procedure this document replaces with a continuous one.
- [`crates/deis-control/deis-control.example.toml`](../crates/deis-control/deis-control.example.toml) -
  every setting, with the restore set written out beside `data_dir`.
- [`docs/trust-boundaries.md`](trust-boundaries.md) - the file permissions the
  key material and the machine identity need.
- [ADR-012](adr/012-pluggable-persistence.md) - the decision, the RPO/RTO
  targets, the checkpoint rule, and the durability trade-offs behind WAL and
  `synchronous = FULL`.