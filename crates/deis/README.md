# deis — the Deislet developer CLI

`deis` is the command a person uses. It has two halves, and keeping them
apart is the point:

- **The project on disk.** `deis init`, `deis dev`, `deis validate` and
  `deis build` need no platform at all. It always creates one verified Edge
  Canon Build Output first and derives each selected target's payload from
  that same parent: `deislet` and `cloudflare` by default (whichever
  `.config.json` enables), the frozen `deno` and `tencent` only when named
  with `--target`.
- **The locked standard registry.** `deis targets list|inspect` reads the same
  generated target-status JSON as the product documentation and Web console;
  it does not infer support from an adapter being present.
- **A Deislet platform.** `deis deploy`, `deis versions`, `deis deployments`,
  `deis provider-deployments`, `deis undeploy`, `deis rollback`, `deis status`,
  `deis routes`,
  `deis domains`,
  `deis secret`, `deis logs`, `deis tail`, `deis trace`,
  `deis kv`, `deis db` and `deis blob` talk to a `deis-control`; for logs and
  traces, also to a `deis-telemetry`; and for the last three, to a
  `deis-store` whose address and capability token the control plane hands
  out.

`deis deploy` deploys to this platform, or with `--target cloudflare` to
Cloudflare through the control plane's Cloudflare provider driver. The frozen
Deno and EdgeOne targets are not deployable from this CLI and `--target`
refuses them.

Compilation, validation and target code generation all live in `deis-build`;
this crate is its command-line shell plus everything above.

## Install

```bash
cargo build -p deis -p deis-runtime      # deis dev starts the second one
cargo install --path crates/deis
```

`deis dev` needs the `deis-runtime` binary. It is looked for, in order: the
`--runtime` flag, `$DEIS_RUNTIME_BIN`, next to the `deis` binary itself, then
`PATH`.

## Configuration

Three places, in this order of precedence:

1. a flag on the command (`--app`, `-e/--environment`);
2. an environment variable;
3. the project file `deis.toml`, which belongs in the repository.

```toml
# deis.toml — commit this. It holds addresses, never a credential.
[control]
endpoint = "http://127.0.0.1:8085"

[telemetry]
endpoint = "https://127.0.0.1:8092"

# The authority deis-store, deis-queue and deis-telemetry certificates chain
# to. A public certificate, so it is safe here; a relative path is read from
# this directory.
[services]
ca = "deploy/ca.crt"

[defaults]
app = "shop"           # falls back to "name" in .config.json
environment = "production"
```

| Setting | Variable |
|---|---|
| `control.endpoint` | `DEIS_CONTROL_ENDPOINT` |
| `control.token` | `DEIS_CONTROL_TOKEN` |
| `telemetry.endpoint` | `DEIS_TELEMETRY_ENDPOINT` |
| `services.ca` | `DEIS_SERVICES_CA` |
| `defaults.app` | `DEIS_APP` |
| `defaults.environment` | `DEIS_ENVIRONMENT` |

deis-store, deis-queue and deis-telemetry serve TLS only. `deis kv`, `db`,
`blob`, `queue`, `logs`, `tail` and `trace` check each server's certificate
against `services.ca` and refuse one that does not chain to it or that names
another kind of service, so the capability a command carries is never sent to
the wrong machine or in the clear. An `http://` address is refused unless it
is a loopback IP.

### The credential never goes in the project

A token committed once is a token that has to be rotated, so `deis` will not
write one into a file inside the project — and refuses to *use* one it finds
in `deis.toml`, telling you to rotate it instead.

```bash
deis config set control.token <value>
```

writes it to `~/.config/deis/credentials.toml` (or `$XDG_CONFIG_HOME/deis/`,
or `$DEIS_CONFIG_HOME`) with `0600` permissions, keyed by the control-plane
address, so one machine can hold credentials for a staging and a production
platform at once. `deis config list` shows every setting, where it came from,
and which variable overrides it — it never prints the token itself.

`deis logs`, `deis tail` and `deis trace` need nothing further. Each asks
the control plane for a 15-minute read capability for the environment it is
about (or every environment of the application, with `--all-environments`),
gated on `read` over that same scope and recorded in the audit log, and
presents it to deis-telemetry, which returns only those environments'
records. So they need the control plane reachable and a control token, like
every other command.

## Commands

Human output by default. `--json` is accepted by every command that answers
with data, and prints one JSON document on stdout and nothing else.
`deis tail --json` prints one JSON object per line, because its output has no
end.

The environment is `-e, --environment` everywhere, never `--env`: "env" in
this codebase already means an application's variables (`context.env`).

### `deis init [NAME]`

Create a project: `functions/`, `public/`, `.config.json`.

### `deis dev`

Run the project locally **in a real `deis-runtime`**. The bundle is built, a
node is started, and the bundle is handed to it with the same
`ApplyDeployment` call the control plane makes — so routing, the asset index,
`context.params` and `context.env` behave exactly as they will in production.
Requests to `--port` are forwarded to that node with the two headers
`deis-proxy` would have stamped on.

```bash
deis dev                                   # http://127.0.0.1:8000
deis dev -p 3000 --store http://127.0.0.1:8090 --queue http://127.0.0.1:8091
deis dev -s API_KEY=local-value            # stands in for a platform secret
deis dev --no-watch --debug
```

What is genuinely absent locally is named in the banner rather than faked:
without `--store` and `--queue` there is no KV, database, blob or queue, and
`ctx.services.*` fails exactly as it would on a node that was never given
their addresses. Nothing fires cron locally either — scheduling is the
control plane's job.

The local environment name defaults to `development`, not `production`, so a
`--store` pointed at a real `deis-store` cannot write into the production
tenant by accident.

### `deis validate [--strict]`

Check the project against the specification.

### `deis build [--target …]`

Compile one content-addressed canonical artifact, then derive the selected
provider payloads. `--target deislet` produces
`dist/providers/deislet/<payload-digest>/bundle.zip`, the bundle a version is
made of; `--target cloudflare` produces `wrangler.toml`, the Worker bundle and
`binding-plan.json`; `--target all` also builds the frozen targets. Without
`--target` it builds the enabled non-frozen targets. The command prints the canonical and derivation identities.
Provider packaging fails before output when a declared standard feature has no
implemented mapping; it never silently removes that feature.

### `deis deploy`

Build, create the application and environment if they are absent, upload the
bundle as an immutable version, and point the environment at it.

```bash
deis deploy
deis deploy -e preview --note "trying the new cache"
deis deploy --no-build            # use the bundle the last build left
deis deploy --bundle path/to/bundle.zip
```

It prints the version number, the digest, and which nodes took it. **A node
that did not take it is printed and fails the command**: the pointer moved,
but that node is still serving what it had, and a green deploy that half the
fleet ignored is exactly the failure this is for.

A byte-identical bundle reuses the existing version instead of burning a new
number, and says so.

```bash
deis deploy --target cloudflare
deis deploy --target cloudflare --provider-target cf-eu   # when several are granted
```

With `--target cloudflare` it builds the Deislet bundle and the Cloudflare
payload from one canonical artifact, records the version (without moving any
self-hosted pointer), uploads the payload, starts a provider deployment on the
Cloudflare provider target the environment is granted, and follows it until
it settles; a failed or cancelled workflow fails the command. Setting up the
driver, credential, target and grant is in
[`docs/operations/cloudflare.md`](../../docs/operations/cloudflare.md). This
path is verified against a fake `wrangler`, not yet a real account.

The deployment-time `config` values in `.config.json` go with the request;
secret values are set on the environment with `deis secret set` and reach the
driver from the control plane, never from the CLI.

### `deis undeploy [--target …] [--purge-data …]`

Take the environment's deployment on the provider (`--target cloudflare`) down: the control plane
records an `undeploy` operation that runs the driver's `delete` against the
environment's latest provider deployment, and the command follows it until it
settles. The Worker goes; the environment's provider data - KV namespaces,
databases, buckets and queues - is kept. `--purge-data` destroys it too, and
takes the environment's name again as the consent: a value naming any other
environment is refused. `--provider-target` chooses the target when the
environment has deployed to more than one.

### `deis targets list|inspect`

Show each target's implementation-evidence maturity, whole-standard
certification, compliance and support status from the locked Edge Canon
registry. `list --json` returns the generated document used by the Web console
and product documentation; `inspect --json` returns one target together with
the same standard and registry provenance. Artifact generation or deployment
evidence is not displayed as support.

### `deis versions [--limit N]`

Every version of the application: number, digest, when it was created, and
which environments currently point at it.

### `deis deployments [--limit N]`

One environment's history, newest first: when its pointer moved, to which
version id, and the note whoever moved it left. `deis status` says what runs
now and `deis versions` says what the application has ever built; this says
when a particular environment changed, which is where an incident starts.

One request, so `--limit` is capped at 1000 — that is one page of the list
route. The line under the table says how many rows are behind the ones
printed, and where to look when they are further back than one page reaches.

### `deis provider-deployments list|get|cancel|rollback`

One environment's provider deployments: the durable workflows that drive a
provider's own cloud, not the pointer moves `deis deployments` lists. A
provider deployment runs for as long as that cloud takes, so unlike a pointer
move it can be asked to stop, and once finished it can be undone.

- `list [--limit N]` prints them newest first: operation id, kind (`deploy`,
  `rollback` or `undeploy`), state, phase, progress, what a rollback was rolled back from,
  and when a cancel was requested. One request, so `--limit` is capped at
  1000 - one page of the list route - and the line under the table says how
  many rows are behind the ones printed, exactly as `deis deployments` does.
- `get <operation-id>` prints one workflow whole: the operation, what it is
  bound to, and every step with the child operation that ran it.
- `cancel <operation-id>` asks a running workflow to stop. Steps that have not
  run do not run and what is reversible is cleaned up; steps that already took
  effect stay in effect - undoing those is `rollback`. A finished workflow is
  refused.
- `rollback <operation-id>` drives the provider back to what a succeeded
  deployment left behind. It records a **new** operation rather than undoing
  the named one, so the id it prints is a different one; `--wait` follows that
  new operation to a terminal state instead of returning as soon as the
  control plane has accepted it. `--json` names the new operation either way:
  a wait that gives up before the workflow settles leaves `providerDeployment`
  null and says why in `waitError`, and the command still exits non-zero.

The operation id is the OPERATION column of `list`; anything that is not a
canonical UUIDv7 is refused before a request is sent.

### `deis rollback [--to N]`

Move an environment's pointer back — to a version number, or with no `--to`
to the previous distinct version it ran. Prints what it moved **from** and
**to**, and where the change landed. Nothing is deleted, so the rollback is
itself reversible.

### `deis status`

What each environment runs (from the catalog: version, digest, when, note)
and which nodes are serving it (asked of the nodes themselves: environment,
state, request and error counts). Printed as two tables because they are two
different claims, and a node that never took the last deployment is where
they disagree.

### `deis routes list`

One environment's routes in the order the edge applies them: highest priority
first, then by path prefix, so the first row that matches a request is the row
that wins. A route with no host prints as `(no-host)` — it answers for every
name no more specific route claims, which is the widest claim on the platform,
not the mildest.

### `deis domains list`

The hostname registry: which application, in which environment, may claim
which name. A hostname is the one global namespace on this platform, so the
registry is platform-wide and the control plane refuses every route claim it
does not cover. Reading it needs `admin:*/*`.

Read-only here on purpose. Granting a name decides which tenant receives
somebody's cookies, and withdrawing one breaks whatever is already serving on
it; neither belongs a keystroke away from a listing.

### `deis secret set|list|rm`

```bash
deis secret set API_KEY                    # value read from stdin
printf %s "$VALUE" | deis secret set API_KEY
printf %s "$PREVIEW_VALUE" | deis secret set API_KEY -e preview
deis secret list
deis secret rm API_KEY
```

`set` refuses an echoing terminal and has no positional value form, keeping
plaintext out of shell history and process listings. `list` shows names,
immutable revisions and times. There is no command that prints a value — the
control plane has no endpoint that returns one. A secret is not part of the
bundle, so changing one creates no application version; the running version
is delivered again with a new environment snapshot, and where that landed is
printed.

### `deis logs` and `deis tail`

```bash
deis logs --since 2h -l warn
deis logs --since 2026-08-29T10:00:00Z --until 2026-08-29T11:00:00Z
deis logs --trace-id 4f2c… --limit 500
deis tail -e preview
deis tail --all-environments --json
```

`--since`/`--until` take a duration back from now (`30s`, `15m`, `2h`, `3d`)
or an RFC 3339 instant. Entries print oldest first, the way a log reads.

`deis-telemetry` stores an application's lines under one `app_id` with the
environment as a field, on purpose, so `-e` is a filter this CLI applies;
`--all-environments` turns it off. Lines from components that are not serving
one particular environment are always kept — they are usually the ones that
explain why an application is not running.

Neither command needs a control plane configured: reading logs is not a
control-plane operation. `deis trace` is the same — it reads deis-telemetry.
`deis kv`, `deis db` and `deis blob` do need one, because that is where the
capability token comes from.

`Ctrl-C` ends a tail cleanly.

### `deis trace`

One row per invocation, from what the nodes report to deis-telemetry.
`deis logs` shows what an application *said*; this shows what the platform
*decided*.

```bash
deis trace --outcome failed --since 30m
deis trace --kind queue --min-duration 500
deis trace --status 5xx --limit 20
deis trace --trace-id 4f2c…          # the same id deis logs --trace-id takes
deis trace --follow
```

`--outcome` is one of `ok`, `app-error`, `timeout`, `rate-limited`,
`memory-limit`, `cpu-limit`, `internal-error`, or `failed` for every one but
`ok`; it is repeatable. So is `--kind` (`http`, `queue`, `cron`,
`websocket`). `--status` takes a code (`500`) or a class (`5xx`) and is
applied by this CLI rather than by deis-telemetry. `--since`/`--until`,
`--target-contains` and `--min-duration` narrow further; `--follow` streams
from now on and is exclusive with the range filters.

The distinction that makes this worth having: an invocation the application
threw in and one the platform cut off are different outcomes, so "show me
what we killed today" is a question with an answer.

### `deis kv`, `deis db` and `deis blob`

The data plane, for the day an operator has to look at what an application
actually stored.

```bash
deis kv list --prefix user: -e preview
deis kv get session:abc --file ./out.bin
deis kv put flag:beta on --ttl 3600
deis db query "SELECT id, email FROM users WHERE id = ?" --param 42
deis db execute "UPDATE users SET email = ? WHERE id = ?" --param a@b.c --param 42 -y
deis blob list avatars/ --delimiter / --all
deis blob head avatars/42.png
deis blob get avatars/42.png --file ./42.png
deis blob get video.mp4 --offset 0 --length 65536
deis blob put avatars/42.png --file ./42.png --content-type image/png --if-match <etag>
```

Values go in from a file (or on stdin) and come out to a file, so binary
content never has to pass through a shell; a value that is not valid UTF-8 is
described — size and a hex preview — rather than printed at a terminal.
`--param` is given once per `?` in order, and each one is read as JSON when it
parses (`42`, `true`, `null`, `"quoted"`) and as a plain string otherwise.

The three destructive verbs — `kv delete`, `db execute`, `blob delete` — ask
for the tenant to be typed back before they run. `-y`/`--yes` skips that, for
scripts. These commands do not talk to deis-store on their
own authority: they ask the control plane for this `(app, environment)`'s
capability token first, with the same credential that deploys, and the
control plane answers with the tenant name, the store address and the token.
Nothing is minted by hand.

`deis blob` offers every verb `BlobStore` answers, and a verb the store has
that the command line cannot reach is an RPC delivered to nobody:
`blob list` (a prefix as its argument, plus `--delimiter`, `--limit`,
`--cursor` and `--all`),
`blob head`, `blob get --offset/--length/--suffix` for one slice of an object,
and `--if-match` / `--if-none-match` on `blob put` and `blob delete` for a
conditional write against the object's etag.
`blob_offers_every_verb_the_store_answers` in `src/main.rs` is the parse-level
half of that; `scripts/demo.sh` runs the same commands against a live store.

One shape is deliberately missing, because it is a session rather than a call:
`deis db` has no `batch` or interactive transaction — an application gets both
through its declared `context.env.<DATABASE_BINDING>`, the command line does
not.

### `deis config set|get|unset|list`

See **Configuration** above.

## Exit codes

`0` on success, `1` on failure. Every error says what to do next, not only
what failed. A deploy or a secret change that some node did not take exits
non-zero even though the catalog was updated.

## Related

- [`docs/architecture.zh.md`](../../docs/architecture.zh.md) — why the system is split this way
- [`docs/implementation-plan.zh.md`](../../docs/implementation-plan.zh.md) — the data model, bundle format and proto contracts
- [`deis-build`](../deis-build/README.md) — the compiler this CLI drives
- [Edge Canon](https://github.com/deislet/edge-canon) — the specification an application is written against
