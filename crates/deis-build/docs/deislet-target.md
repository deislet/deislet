# The deislet target

`deis-build build --vendor deislet` produces an **application bundle**: one zip,
which is the only carrier of a version on the deislet platform. The format is
defined in [`docs/implementation-plan.zh.md` section 2](../../../docs/implementation-plan.zh.md).
deis-build writes it, deis-control verifies it, deis-runtime loads it, and all
three work from the same definition in `deis-bundle`.

The other three targets emit somebody else's deployable - a Worker, a Deno
Deploy project, an EdgeOne function. This one emits the platform's own.

## Output

```
dist/deislet/bundle.zip
├── manifest.json       what the app is, and everything a node needs before it loads it
├── worker/index.js     the bundled entry module, one ESM file
└── assets/             static files, directory structure preserved
```

Nothing else is left behind: the staging directories (`src/`,
`_deis_internal/`) are removed once the archive is written, because a loose
copy beside the archive is stale the moment the next build runs.

## Routes come from the file tree

edge-canon says the file tree *is* the route table. A handler's path under the
function root decides which requests reach it:

| File | Pattern |
|---|---|
| `functions/index.ts` | `/` |
| `functions/api/hello.ts` | `/api/hello` |
| `functions/users/[id].ts` | `/users/:id` |
| `functions/users/[id]/index.ts` | `/users/:id` |
| `functions/api/[[catch]].ts` | `/api/*` |

Patterns go into `manifest.json` as `routes[].pattern`, spelled exactly the way
`RouteRule.pattern` is spelled on the wire.

**The table is ordered by specificity**, most specific first, so the generated
matcher can take the first pattern that fits. The rule compares two patterns
segment by segment - a literal beats a `:name`, which beats a catch-all - and a
pattern carrying a catch-all is the last resort whatever else it pins down;
otherwise `/api/*` would swallow every route below it. Where the shape leaves
two patterns tied (`/:org/settings` and `/:user/settings`), the pattern text
decides, so the order does not depend on directory walk order, which differs
between machines; the bundle's sha256 is part of a version's identity, so
nothing in it may vary on its own.

The rule is `deis_bundle::sort_by_specificity`, and deis-runtime ranks the
manifest's patterns by the same one. Two spellings of it would let a request
reach a handler the built table does not point at, and the only symptom would
be a handler that never runs.

`methods` is always empty, meaning any method. An edge-canon handler decides
for itself what to do with a method, and the file tree has no place to say
otherwise.

## What the entry module does

deis-runtime keeps one default export per app and calls it once per request,
and it **builds the context itself** (`request`, `env`, `services`,
`waitUntil`) - it has no idea an app has more than one handler. So the
generated entry module is what turns a file tree into an app:

1. picks the handler whose pattern fits the path;
2. fills `context.params` from its own match. The runtime matches the same
   patterns, catch-all included, but a manifest pattern keeps only the `*`, so
   `params['*']` is the only key it can promise; this module still knows the
   file was written `[[catch]]`, and publishes the tail under that name too;
3. adds the conveniences edge-canon promises: `path`, `method`, `query`,
   `cookies`, `log`;
4. answers 404 when nothing matched.

`context.env`, `context.services` and `context.waitUntil` are passed through
untouched.

## The `.config.json` keys this target reads

```json
{
  "name": "my-app",
  "functionRoot": "./functions",
  "assets": { "dir": "./public" },
  "environment": {
    "variables": { "LOG_LEVEL": "info" },
    "secrets": ["API_KEY"]
  },
  "services": {
    "kv": { "enabled": true },
    "database": { "enabled": true },
    "blob": { "enabled": false },
    "queue": { "enabled": false }
  },
  "cron": ["0 */6 * * *"],
  "queues": [{ "name": "jobs", "batchSize": 10 }],
  "vendors": { "deislet": { "enabled": true } }
}
```

- `name` is the `app_id` the version is filed under in the control plane.
- `assets.dir` defaults to `./public` when the key is absent; a missing
  directory simply means no static files, not a build failure.
- `secrets` carries **names only**. Values stay with the control plane and are
  delivered to a node at deploy time.
- `cron` and `queues` are copied verbatim into the manifest; scheduling is the
  control plane's job.

## The capability set says what works today

`Platform::Deislet` is filled in from the code that exists, not from the
roadmap. Among the extended features these are supported:

- **SQL Database** and **Object Storage** - deis-store serves both, and
  deis-runtime builds the isolate's client from the manifest's `services`, so
  `context.services.database` and `context.services.blob` reach the server.
- **Queues** - deis-queue is its own process; `send` reaches it, and a node
  polls it for every queue the manifest declares a consumer for.
- **Cron** - the control plane holds one schedule per (app_id, environment) and
  triggers a node when one is due; the node runs the app's `scheduled` export.
- **WebSockets** - deis-runtime bridges server-side sockets into the isolate
  and deis-proxy tunnels the upgrade. The two halves have never been tested end
  to end, which `docs/architecture.zh.md` section 11 records as an open item.

Two kinds of unsupported feature remain, treated differently at build time:

- **Pending** - specified, half built. These **warn and do not stop the
  build**: the declaration belongs in the manifest, where the control plane
  records it, and the binding arrives without the app being rewritten. **The
  list is empty today**; the mechanism stays for the next half-built service.
- **Not coming** (Durable Objects, Node Runtime, SSR/ISR, Workers AI,
  BroadcastChannel) - nothing in the platform's shape provides them, so a build
  that needs one fails.

## Reproducibility

The same input has to produce the same bytes, or redeploying unchanged source
creates a new version every time. So:

- zip entries are written in sorted order, not discovery order;
- timestamps are fixed at the zip epoch (1980-01-01) rather than read from the
  clock (`FileOptions::default()` reaches for the clock with the `time`
  feature on);
- permissions are fixed at 0644 rather than following the build host's umask;
- the module bundler's working directory is pinned to the project root, so the
  module paths it writes into the output are relative to the project and carry
  no build-host path.

`crates/deis-build/tests/deislet_bundle.rs` covers both halves: one build in
each of two different temporary directories with the digests compared, and two
builds in the same directory with the digests compared.
