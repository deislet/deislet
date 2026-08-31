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

`manifest.json` carries a `site` block when the project has one — see **What a
static site says about itself** below. `_redirects` and `_headers` are read out
of the assets directory and **not** copied into `assets/`: their content is in
the manifest, and shipping them as files as well would publish the site's own
rules at `/_redirects`.

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

## What a static site says about itself

Three things a site can do without writing a function. All three are parsed
here, at build time, and travel in the manifest's `site` block; deis-runtime
compiles them once when the version loads and applies them on every request
that misses the asset index. **A rule that is wrong fails the build**, naming
the file, the line number and the line's text — an author who mistyped a
redirect finds out here rather than from a visitor.

### `_redirects`

One rule per line, at the root of the assets directory. First match wins, in
the order the author wrote them: nothing sorts this table, because an author
who put a broad rule above a narrow one meant the broad one to win.

```text
/old-post          /blog/new-post        301
/blog/*            /news/:splat          301
/news/:year/:slug  /archive/:year/:slug  302
/app/*             /app/index.html       200
/private/*         /404.html             404
```

The status defaults to `302`, and three of the seven allowed values do
something other than redirect: **200** is a rewrite (the URL stays put and the
target's bytes are served under it — this is how a pretty URL or a hand-written
SPA fallback is spelled), **404** serves the target with a 404 status, and the
rest are ordinary redirects with a `Location`. At most 2000 rules, because the
table is scanned on every request that names no file.

An absolute `http(s)://` target is allowed **for a redirect and refused for a
rewrite**, and only when the host it names is a constant. Refused shapes: a
capture anywhere in the scheme or authority (`/r/:host  https://:host/`, which
would make the site somebody else's open redirector), userinfo, a scheme that
is not http or https, and a protocol-relative `//host/path`. A rewrite to an
absolute URL is refused outright: a rewrite serves bytes out of *this* bundle,
so honouring one would have the node open a socket the author chose, from the
asset path, past the platform's single outbound decision point.

### `_headers`

A path pattern on its own line, its headers indented under it.

```text
/*
  X-Frame-Options: DENY

/assets/*
  Cache-Control: public, max-age=31536000, immutable

/downloads/:file
  Content-Disposition: attachment
  ! X-Robots-Tag
```

`Name: value` sets a header and `! Name` removes one. **Every matching block
applies and a later one wins** — the opposite of `_redirects`, and deliberately
so: headers compose (a site-wide security block plus a per-directory caching
block) while a redirect is one decision about where a request goes.

`!` reaches only what these blocks set. Refused at build time, by name, with
the line number: the framing and hop-by-hop headers (`content-length`,
`transfer-encoding`, `connection`, `keep-alive`, `upgrade`, `te`, `trailer`,
`proxy-authenticate`, `proxy-authorization`, `host`), `content-encoding` (the
node and the proxy decide whether a response is compressed), `etag` (computed
from the file's own digest, and a constant one pinned by an author would have
caches keep the old file forever after a deploy), and anything beginning
`x-deis-` (the platform routes and authorises on those).

### The not-found behaviour

`assets.notFound` in `.config.json`, one of three, spelled the way Cloudflare
Pages spells them so an author arriving from there already knows the names:

| Value | What a path nothing claimed gets |
|---|---|
| `none` | the request goes on to the isolate, which answers 404 itself |
| `404-page` | `404.html`, with status 404 |
| `single-page-app` | `index.html`, with status 200, so a client-side router can read the path |

Unset, it is inferred: `404-page` when the bundle carries a `404.html`, `none`
otherwise. Inferring the SPA behaviour is not possible the same way — every
site has an `index.html`, and only the author knows whether an unmatched path
is a client-side route or a mistake. A declared value naming a file the bundle
does not carry fails the build.

### The pattern language, and one deliberate narrowing

`_redirects` sources and `_headers` patterns are written in the same language a
function route is: literal segments, `:name` for exactly one segment, and `*`
as the whole last segment for the rest of the path. A wildcard *inside* a
segment — `/*.jpg` — is refused at build time rather than half understood,
because a second dialect that only worked here would be a second thing to keep
in agreement forever. Per-extension headers are written as a directory: put the
files under `/img/` and match `/img/*`.

### The way back in, from a function

A handler that ran anyway and wants the asset layer's answer for a path of its
own choosing calls `env.ASSETS.fetch(request | url | path)`. It runs the same
pipeline with one difference — the function routes have already had their turn,
so the fallback applies whatever it says — and always answers something, so a
path that names nothing comes back as a 404 the handler can return rather than
as an exception every handler would have to invent a response for.

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
  "assets": { "dir": "./public", "notFound": "single-page-app" },
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
  "limits": { "timeoutMs": 5000, "memoryMb": 64 },
  "vendors": { "deislet": { "enabled": true } }
}
```

- `name` is the `app_id` the version is filed under in the control plane.
- `assets.dir` defaults to `./public` when the key is absent; a missing
  directory simply means no static files, not a build failure.
- `assets.notFound` is the not-found behaviour above; absent, it is inferred
  from whether a `404.html` is there.
- `secrets` carries **names only**. Values stay with the control plane and are
  delivered to a node at deploy time.
- `cron` and `queues` are copied verbatim into the manifest; scheduling is the
  control plane's job.
- `limits` is a request to be held to **less** than the node allows, and only
  ever less: `memoryMb`, `timeoutMs`, `cpuTimeLimitMs`, `rateLimitOpsPerSec`
  and `resourceTableMax`, each optional and each meaning "the node's default"
  when absent. It is translated into the manifest's own `limits` block, which
  the control plane checks at upload and every node clamps against its own
  ceiling - a project asking for more gets the node's number and a log line
  naming both. A misspelled key fails the build rather than being dropped: a
  ceiling an author believes in and that was never in force is the failure this
  whole key exists to avoid. A zero is refused for the same reason, because
  zero is what "unset" looks like on the wire.

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
