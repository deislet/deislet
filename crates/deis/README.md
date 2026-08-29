# deis — the Deislet developer CLI

`deis` is the command a person uses. It has two halves, and keeping them
apart is the point:

- **The project on disk.** `deis init`, `deis dev`, `deis validate` and
  `deis build` need no platform at all. `deis build --vendor cloudflare|deno|tencent`
  produces what that vendor's own tooling deploys; this CLI never pushes to
  them.
- **A Deislet platform.** `deis deploy`, `deis versions`, `deis rollback`,
  `deis status`, `deis secret`, `deis logs` and `deis tail` talk to a
  `deis-control` and, for logs, a `deis-telemetry`.

`deis deploy` targets **this** platform only. There is deliberately no
`--vendor` on it: it could never have pushed to Cloudflare or Deno, and a
flag suggesting otherwise is worse than no flag.

Compilation, validation and vendor code generation all live in `deis-build`;
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
endpoint = "http://127.0.0.1:8092"

[defaults]
app = "shop"           # falls back to "name" in .config.json
environment = "production"
```

| Setting | Variable |
|---|---|
| `control.endpoint` | `DEIS_CONTROL_ENDPOINT` |
| `control.token` | `DEIS_CONTROL_TOKEN` |
| `telemetry.endpoint` | `DEIS_TELEMETRY_ENDPOINT` |
| `defaults.app` | `DEIS_APP` |
| `defaults.environment` | `DEIS_ENVIRONMENT` |

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

### `deis build [--vendor …]`

Compile. `--vendor deislet` produces `dist/deislet/bundle.zip`, the bundle a
version is made of; the other vendors produce their own output for their own
tooling.

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

### `deis versions [--limit N]`

Every version of the application: number, digest, when it was created, and
which environments currently point at it.

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

### `deis secret set|list|rm`

```bash
deis secret set API_KEY                    # value read from stdin
printf %s "$VALUE" | deis secret set API_KEY
deis secret set API_KEY value -e preview
deis secret list
deis secret rm API_KEY
```

`list` shows names and times. There is no command that prints a value — the
control plane has no endpoint that returns one. A secret is not part of the
bundle, so changing one creates no version; the running one is delivered
again with the new value, and where that landed is printed.

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
control-plane operation.

`Ctrl-C` ends a tail cleanly.

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
