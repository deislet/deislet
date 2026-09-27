# Deploying the canary node: resources and hardening

This is the node sizing behind `scripts/canary/systemd/` — what bounds each
service, where the numbers come from, and what to do differently in a
container. The trust-model side of the same files is in
[trust-boundaries.md](trust-boundaries.md).

## The two kinds of ceiling

Every ceiling the runtime applies to an application — the JS heap limit, the
subrequest allowance, the concurrent-outbound-connection ceiling, execution
deadlines — is applied by code running *inside* the process, out of memory the
same kernel gave it. They bound a well-behaving application. A backstop is
the ceiling that holds when those cannot: the kernel refusing the process
more memory than its cgroup allows. That is why the runtime checks at startup
whether it runs under a cgroup v2 `memory.max` and answers `degraded` with
「resource backstop missing」 when it does not — a node that cannot be stopped
by anything but the machine's own death does not make the promise `ready`
makes.

## The slice and the per-service ceilings

All deislet services join `deislet.slice`, so the platform as a whole has one
accountable share of the machine. Each unit then sets its own `MemoryMax`:

| Unit | MemoryMax | What it is |
|---|---|---|
| `deis-runtime.service` | `MemoryHigh=1500M`, `MemoryMax=2G` | The only service whose memory scales with the applications on it |
| `deis-control.service` | `1G` | The database and the console |
| `deis-store.service` | `1G` | SQLite caches plus the blob store's page cache |
| `deis-proxy.service` | `512M` | Connection state |
| `deis-queue.service` | `512M` | The journal |
| `deis-telemetry.service` | `512M` | Buffers |

`deis-runtime.service` also sets `MemoryHigh` (soft throttle before the hard
stop), `OOMPolicy=stop` (a trip ends the *service*, which systemd restarts —
an isolate that beat every in-process limit has forfeited the benefit of the
doubt), `TasksMax=256`, `CPUWeight=100`, `RestrictAddressFamilies`,
`SystemCallFilter=@system-service` (with `@privileged @resources` removed),
and `ProtectHome=true`. `MemoryDenyWriteExecute` is deliberately **off**: V8
writes generated code, and the flag forbids exactly that.

## Sizing deis-runtime

The runtime's memory is one isolate heap per application, times two — V8
commits roughly twice the live heap between the allocation limit and its own
bookkeeping — plus what the process itself needs:

```text
deis-runtime memory ceiling
  ≈ threads × default_memory_limit_mb × 2      (isolate heaps, committed on demand)
  + 150 MB                                     (process, connection buffers, snapshot)
  + headroom for spikes                        (at least 25%, more on a bursty node)
```

The default `default_memory_limit_mb` is 256 (`default_execution_limits` in
`deis-runtime.toml`), and one runtime thread serves one application at a
time; the thread count is the number of applications the node is sized for
(the pool sizes itself from the machine's CPUs, so a node meant to carry 8
applications on a 4-CPU machine plans around 8 threads of ceiling, not 4).
For the canary's small fleet that is:

```text
8 threads × 256 MB × 2 + 150 MB ≈ 4.2 GB  →  raise MemoryMax above 2G
2 threads × 256 MB × 2 + 150 MB ≈ 1.2 GB  →  the shipped 2G leaves room
```

A node carrying more applications, or applications with a raised
`limits.memory`, needs the ceiling recomputed the same way — and the
`--memory` advice below recomputed with it.

## In a container

The cgroup the runtime detects at startup is whatever bounds the container,
so the same arithmetic applies to `docker run --memory` / `podman --memory`:

- Set `--memory` to the ceiling computed above, and `--memory-swap` equal to
  it (swap participation turns a clean memory-limit refusal into thrashing).
- `--pids-limit` is the container's `TasksMax`; the same 256-per-runtime
  arithmetic applies, plus the other processes in the container.
- One process per container, and no `deislet.slice`: compose the platform
  from per-service containers, each with its own `--memory` from the table
  above.
- If nothing bounds the container, the runtime says so at startup —
  readiness reports degraded with the missing backstop named — rather than
  quietly promising a node it is not.

## Verifying the units

`systemd-analyze verify` on the shipped files (the binaries are not
installed where verify runs, so point the ExecStart paths at placeholders):

```bash
for unit in scripts/canary/systemd/*.service; do
  sed 's|/usr/local/bin/|/tmp/verify-bin/|' "$unit" > "/tmp/verify-bin/$(basename "$unit")"
done
systemd-analyze verify /tmp/verify-bin/*.service /tmp/verify-bin/deislet.slice
```

In CI the same check runs where systemd is present; where it is not (a
minimal runner without systemd), the check is skipped and this document is
the record that the shipped files verified clean at the time they changed.
