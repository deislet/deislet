# Release notes - 0.1.0

The first release of Deislet as a whole: control plane, runtime nodes, edge
proxy, the three data services, the CLI and the compiler. It ships with the
list below, written before the tag rather than after the first incident.
Everything on it was found by the project's own adversarial review rounds
and is either fixed here, mitigated as stated, or a product decision that is
recorded rather than a defect that is pending.

## What the evidence is, and what it is not

Every number in this document comes from a run on one workstation:

| evidence | result at the tag |
|---|---|
| `cargo test --workspace` | see the commit that carries this tag; the count is in its message |
| `scripts/demo.sh verify` | 95 of the 95 checks the script lists |
| `scripts/test-demo-witnesses.sh` | all pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo audit` | 0 advisories, 4 recorded judgements in `.cargo/audit.toml` |

Continuous integration: **observed green once, on the commit this tag
points at.** `.github/workflows/checks.yml` runs four jobs on every push and
pull request - the document-path check, the demo witness tests, the
workspace job (fmt, clippy, build, test, with the dashboard built first) and
the demo job (start, verify, stop). Its first four runs are worth knowing
about, because they changed the code: the runner had no `protoc`; its stable
Rust was two minor versions ahead of the workstation and its clippy refused
tonic-generated code (the toolchain is pinned in `rust-toolchain.toml` now);
a debug build with full debug info filled the runner's disk (CI builds carry
none); and on the runner's dual-stack `localhost` the outbound policy let
`::1` through where it dropped `127.0.0.1` - a real defect, fixed in
`6671657` together with a diagnostic that named the wrong reason for a
stream cut off by the op ceiling. One green run is one run: the numbers
above were reproduced on `ubuntu-latest`, not established there.

## Known defects

Each entry names the symptom an operator or author would see, and what to do
about it. Severity is the reviewer's, after the fix wave that closed
everything above it.

### Operations

- **A node restarted while the control plane is unreachable serves its
  applications with every secret missing, and stays in rotation.** The
  node's local state carries no secrets by design; on restart it asks the
  control plane for them, and when that fails it loads what it has. The
  proxy's default readiness policy keeps such a node in rotation. Symptom:
  applications answer, but every environment variable that was a secret is
  undefined, and the node's log says the assignment list was withheld.
  Mitigation: set `readiness = "required"` in the proxy's upstream health
  policy for any fleet whose applications depend on secrets, so a node that
  reports degraded is taken out of rotation until the control plane is back.

- **The blob store's start-up sweep only reports orphaned content.** A
  content file with no metadata row is either a crash's leftover or every
  object written after the metadata snapshot that was just restored, and the
  sweep cannot tell which. It logs the count at warn and leaves the files.
  Mitigation: after confirming the metadata is the newest copy, set
  `remove_orphaned_blob_content = true` in deis-store's configuration for one
  start, then remove the setting.

- **`deis-build` does not install the log-line escaper the services do.** It
  is the author's compiler over the author's own tree, so no other tenant's
  bytes reach its log; a project that logs its own control characters sees
  them raw.

### Application authors

- **A WebSocket message listener cannot await a `fetch` or a service call.**
  A message is its own invocation and ends when the listener returns, so an
  `async` listener that awaits either is cut off with a `TimeoutError` saying
  the invocation ended. Storage calls are not swept. Do the work in the
  request that accepted the socket, or from a `waitUntil` there.

- **A `waitUntil` promise registered inside a WebSocket message handler is
  dropped** when the message's invocation ends, with a warning the
  application can see. Same remedy as above.

- **A handler parked on an `await` with no timeout of its own is not cut off
  by the CPU watchdog**, because it is not running bytecode. The wall-clock
  ceiling still applies to the request; a handler that awaits an outbound
  call must give that call a timeout.

- **An uncaught background promise rejection turns a completed 200 into a
  500.** A `fetch` started and not awaited, with no `.catch`, is reported by
  the engine as an event-loop error after the handler has already produced
  its response. Attach a `.catch`, or hand the promise to `waitUntil`.

### Routing and certificates

- **A zone grant's wildcard route answers for a delegated exact name until
  the delegate routes it.** A grant for `*.example.com` and a grant for
  `shop.example.com` to another application coexist; the exact route wins as
  soon as the delegate creates it, and until then the wildcard serves the
  name. Check `GET /api/domains` and the routing table after delegating.

- **One renewal writer per certificate store.** Two proxies sharing an ACME
  store take a store-wide lock; the second skips its pass and says so. Run
  one proxy per store, or accept that only one of them renews.

### Fleet credential

- **The node credential is one shared secret for the whole fleet.** Any
  `node_preshared_keys` value reads every environment's decrypted secrets
  and drives every node's management plane; deis-proxy holds one too; the
  node's data port is a trust boundary. This is a product decision recorded
  in `docs/trust-boundaries.md`, together with the file modes and the
  rotation procedure the code now supports: a node is driven with the key it
  proved at registration, so dropping a value from the list retires whatever
  still uses it. A holder of a leaked value can still re-register with it
  until it is dropped; rotate promptly.

### Product decisions, recorded rather than pending

- **No per-tenant quota** in blob storage, queues or telemetry. One tenant
  can fill the node's disk for every tenant. The per-service READMEs say so;
  size the disk for the fleet you admit, and watch it.
- **A telemetry producer is not bound to the `app_id` it reports.** The
  shared telemetry secret authenticates a producer, not a tenant; a holder
  can write any application's log stream. Recorded in
  `crates/deis-telemetry/README.md`.
- **The scheduler runs on one control plane at a time**, held through a lease
  row; a second control plane on the same database records what it would
  have fired and fires nothing.

## What has not been driven

Ranked by the reviewers after nine rounds; these are the parts of the
system where a defect found tomorrow would surprise nobody:

1. The dashboard (`crates/deis-control/web`) as a running artefact - no
   integration test, one vacuous unit test.
2. Multi-node behaviour beyond a two-node probe: placement, split brain, a
   node returning after an unload it never heard.
3. `deis-queue` depth observability and metrics.
4. The ACME subsystem against a real authority (driven against a stub only).
5. The telemetry and blob stores under sustained hostile load.

## Where the history is

`docs/trust-boundaries.md` states the security model. The per-crate READMEs
state each service's promises and their exceptions. `DEMO.md` lists what the
demo proves, check by check.
