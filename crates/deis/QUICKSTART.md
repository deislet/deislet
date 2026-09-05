# Quick start

From nothing to an application serving traffic on your own platform, and
back again when it goes wrong.

## What you need

- Rust 1.85+ (edition 2024)
- Node.js 18+ — the compiler shells out to it for bundling

Build the two binaries this guide uses:

```bash
cargo build -p deis -p deis-runtime
cargo install --path crates/deis
```

`deis dev` starts a real `deis-runtime`, so that binary has to exist. It is
looked for next to `deis`, on `PATH`, in `$DEIS_RUNTIME_BIN`, or wherever
`--runtime` says.

## 1. Create a project

```bash
deis init my-app
cd my-app
```

```
my-app/
├── .config.json        # what to build and which vendors
├── functions/          # one file per route
│   ├── index.ts        #   → /
│   └── api/hello.ts    #   → /api/hello
└── public/             # static assets, served before any isolate starts
```

A handler is a function of one `Context`:

```typescript
// functions/users/[id].ts  →  /users/:id
export default async function handler(context: Context): Promise<Response> {
  const id = context.params.id;
  return new Response(JSON.stringify({ id }), {
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}
```

The context has exactly `request`, `env`, `params`, and `waitUntil`. Declared
logical resources appear under `context.env.<NAME>`; provider or Deislet
convenience fields are not part of this API.

Two file names under `functions/` are not routes but handlers the platform
calls itself: `_scheduled.ts` (cron) and `_queue.ts` (queue messages).

## 2. Run it locally

```bash
deis dev
```

This builds the bundle and hands it to a real `deis-runtime` — the same
binary that serves production, loaded through the same call the control plane
makes. Routing, the asset index, `context.params` and `context.env` are the
production ones.

```
Serving
  →  http://127.0.0.1:8000
  application  my-app environment development
  loaded       3 route(s), 2 asset(s), sha256 84d8408fe6c2
  node         http://127.0.0.1:41539 (real deis-runtime)
```

Editing a file rebuilds and reloads. A build that fails is reported and the
previous one keeps serving.

The banner also says what is *not* there. Storage is a separate process, so a
declared storage binding cannot work until you point the runtime at one:

```bash
cargo build -p deis-store -p deis-queue
deis-store  --config deis-store.toml  &     # listens on 8090
deis-queue  --config deis-queue.toml  &     # listens on 8091

deis dev --store http://127.0.0.1:8090 --queue http://127.0.0.1:8091 \
         -s API_KEY=local-value
```

`-s/--secret NAME=VALUE` stands in for a value the control plane would have
decrypted and delivered. Nothing triggers cron locally; call the handler
directly to test it.

## 3. Point at a platform

You need a running `deis-control`, and a `deis-telemetry` for logs.
`scripts/demo.sh start` in the repository root brings up a whole one — six
processes with an example application already deployed — if you just want
something to aim at.

```bash
deis config set control.endpoint   http://127.0.0.1:8085
deis config set telemetry.endpoint http://127.0.0.1:8092
deis config set control.token      <token>     # only if the API is guarded
```

The first two go into `deis.toml` next to your project. **Commit that file.**
The token does not go in it — `deis config set control.token` writes it to
`~/.config/deis/credentials.toml` with `0600` permissions, keyed by the
control plane it belongs to. `deis` refuses to use a token it finds in
`deis.toml` at all, and tells you to rotate it.

```bash
deis config list
```

shows every setting, where it came from and which environment variable
overrides it — never the token itself.

## 4. Deploy

```bash
deis deploy
```

```
Deploying my-app to production
  control plane  http://127.0.0.1:8085
  ✓ built dist/providers/deislet/<payload-digest>/bundle.zip (2.7 KB, sha256 a9a8013decdf)
  ✓ created application my-app
  ✓ created environment production
  ✓ uploaded 2.7 KB as version 1

  ✓ production now runs version 1 (sha256 a9a8013decdf)
      ✓ node-01
      ✓ node-02
```

One command: build, create the application and environment if they do not
exist, upload the bundle as an immutable version, point the environment at
it, and push it to the fleet.

A node that did not take the version is printed as `✗` and the command exits
non-zero. That node is still serving what it had, and it is the whole reason
this is worth reading rather than skipping.

```bash
curl http://127.0.0.1:8087/users/42        # through deis-proxy
```

## 5. A second environment

An environment has its own version pointer, its own secrets and its own
storage tenant, so a preview cannot overwrite production's data.

```bash
deis deploy -e preview
deis status
```

```
Environments
ENVIRONMENT  VERSION  DIGEST        DEPLOYED             AGE     NOTE
preview      2        84d8408fe6c2  2026-08-29 15:31:39  0s ago  -
production   1        a9a8013decdf  2026-08-29 15:30:38  1m ago  -

Nodes
NODE     ADDRESS                HEALTH   ENVIRONMENT  STATE    REQUESTS  ERRORS
node-01  http://10.0.0.1:8086   healthy  production   running  1204      0
node-01  http://10.0.0.1:8086   healthy  preview      running  17        0
```

Environment names may not contain a dot: the storage tenant is
`<app>.<environment>`, so `pr-42.1` would split into three parts and every
`kv`, `database`, `blob` and `queue` call of that application would fail —
while the application itself loaded and served traffic. Write `pr-42-1`.
`deis` refuses the name up front rather than letting you find that out later.

## 6. Secrets

```bash
printf %s "$STRIPE_KEY" | deis secret set STRIPE_KEY
deis secret list
```

The value is read from stdin when it is not given as an argument, which keeps
it out of the shell history. It arrives in the handler as
`context.env.STRIPE_KEY`.

`list` shows names and times. Nothing prints a value — the control plane has
no endpoint that returns one. A secret creates no version, so the running one
is delivered again with the new value, and the output says which nodes took
that.

## 7. When it goes wrong

```bash
deis logs --since 15m -l error
deis tail -e preview
```

`deis logs` shows what the application said. When nothing was said — a request
that timed out, or one the platform cut off — ask what the platform decided
instead:

```bash
deis trace --outcome failed --since 15m
deis trace --follow
```

One row per invocation, with the outcome the node chose: `timeout`,
`rate-limited`, `memory-limit`, `cpu-limit`, `app-error`. Each row carries a
trace id, and `deis logs --trace-id <id>` gives that invocation's own lines.

To look at what the application actually stored:

```bash
deis kv list --prefix session:
deis db query "SELECT count(*) FROM users"
deis blob get avatars/42.png --file ./42.png
```

```bash
deis versions
```

```
VERSION  DIGEST        CREATED              AGE     ENVIRONMENTS
3        84d8408fe6c2  2026-08-29 15:31:39  2m ago  preview, production
2        a9a8013decdf  2026-08-29 15:30:38  3m ago  -
1        ec8f5586843b  2026-08-29 15:30:24  3m ago  -
```

```bash
deis rollback              # back to the previous version this environment ran
deis rollback --to 1       # or to a number
```

```
  ✓ production moved from version 3 to version 2 (sha256 a9a8013decdf)
      ✓ node-01
      ✓ node-02
```

A rollback is a pointer move, so it takes about as long as the push. Nothing
is deleted, so the rollback is itself reversible — and the static assets go
back with the code, because they travel inside the version.

## 8. Somebody else's platform

The same source compiles for the vendors:

```bash
deis build --vendor cloudflare      # then: wrangler deploy
deis build --vendor deno            # then: deployctl deploy
deis build --vendor tencent
```

`deis deploy` does not push to any of them, on purpose: each owns its own
credentials and deployment model, and their tools already do it properly.

## Scripting

Every command that answers with data takes `--json`, and prints one document
on stdout and nothing else:

```bash
deis versions --json | jq '.versions[0].number'
deis status --json   | jq '.nodes[] | select(.reachable | not) | .id'
deis deploy --json   | jq -e '.delivery.failed | length == 0'
```

`deis tail --json` and `deis trace --follow --json` print one object per line
instead, because a stream has no end:

```bash
deis tail --json | jq -r 'select(.level == "ERROR") | .message'
deis trace --follow --json | jq -r 'select(.outcome != "ok") | .target'
```

## Where to look next

- [`README.md`](README.md) — every command and flag
- [`docs/architecture.zh.md`](../../docs/architecture.zh.md) — why the system is split this way
- [`DEMO.md`](../../DEMO.md) — the end-to-end demonstration and what it verifies
