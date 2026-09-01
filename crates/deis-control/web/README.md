# deis-control dashboard

The read-mostly web UI the control plane serves from its own port. React 19 +
TypeScript, built by Vite, styled with Tailwind and shadcn/ui components over
Radix primitives.

## How it reaches the browser

There is no separate web server and no separate port. `npm run build` writes
`dist/`, and `crates/deis-control/src/api/static_files.rs` embeds that
directory into the `deis-control` binary with `rust-embed`, serving it from the
same listener that answers the API. So a change here is only live after a
rebuild of the crate.

`dist/` is a build product and is not in git. The crate's `build.rs` creates an
empty one when it is missing, because `rust-embed` cannot expand without the
directory and the crate would otherwise not compile on a fresh clone at all.
A binary built that way is a working control plane with no dashboard: the API
answers normally and the dashboard pages return 404, and the build prints a
warning saying so. To get the dashboard, run `npm run build` here and rebuild
the crate.

## Getting in: the credential

The console has no session of its own. Every call it makes carries the API key
as `X-API-Key`, kept in this browser's `localStorage` and nowhere else.

Which screen you land on is decided by one question the app asks before it
renders anything: `GET /api/metrics/summary`.

- **401** — the control plane has `api_auth_enabled = true` and wants a
  credential, so `src/pages/SignIn.tsx` asks for one. The key is spent on a
  real call before it is committed, so a wrong paste is reported as a refused
  key rather than as a dashboard that will not load.
- **anything else** — straight in. With `api_auth_enabled` off, a caller on
  the machine the control plane runs on is admitted with no credential at all,
  and there is no token to ask anyone for; a key screen there would be a
  locked door with no key cut for it. A 403 or an unreachable control plane
  also goes straight in, because those are conditions the pages themselves
  report.

A 401 arriving later — a revoked key, or one dropped from `api_keys` and the
control plane restarted — drops the stored key and shows the key screen again.
It does **not** navigate. It used to: `lib/api.ts` answered every 401 with
`window.location.href = "/"`, which reloads the console, which starts polling
five endpoints, which answer 401. With `api_auth_enabled = true` and no key
stored, one tab left open on the console reloaded itself continuously and put
1273 refused calls into a control plane in 105 seconds while rendering nothing
a person could type a key into. The 401 is an event now
(`api.onUnauthorized`), and the sign-out button in the sidebar goes through
the same path so there is one way out of a session rather than two.

## Working on it

```bash
npm install
npm run dev      # Vite dev server; it proxies /api to http://localhost:8085
npm run build    # tsc -b && vite build, which is what the crate embeds
npm run lint
```

`npm run build` must pass before the crate is built for release; `tsc -b` is
part of it on purpose, so a type error stops the build rather than shipping.

## Where things live

- `src/pages/` — one file per route: Dashboard, Nodes, Commands, Configs,
  Events, plus `SignIn.tsx`, which is not a route: `App.tsx` renders it
  *instead of* the router, so the polling hooks never mount without a
  credential the control plane has already accepted.
- `src/hooks/useApi.ts` — the polling hooks the pages read from. The dashboard
  polls; there is no server push, and there is no WebSocket endpoint to
  subscribe to.
- `src/lib/api.ts` — the typed request layer: the `X-API-Key` header, the
  401 handling, and `onUnauthorized` / `signOut`, which are how a refusal
  reaches `App.tsx` without a page load.
- `src/components/ui/` — shadcn/ui components, generated and then owned here.

## Two names that are not interchangeable

- A **node** is a registered process — a `deis-runtime` or `deis-proxy` that
  announced itself to the control plane.
- A **worker** is a Cloudflare Worker, which is an application concept and
  never a machine. The UI says "node" for the fleet and never "worker".

The Nodes page shows the two halves separately for the same reason: *control
channels* are the sessions currently attached over gRPC, and the *runtime
fleet* is what the control plane will deploy to and fire cron on. A node can be
in one and not the other, and averaging them into a single "nodes" count hid
exactly that.
