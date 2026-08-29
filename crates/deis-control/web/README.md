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
  Events.
- `src/hooks/useApi.ts` — the polling hooks the pages read from. The dashboard
  polls; there is no server push, and there is no WebSocket endpoint to
  subscribe to.
- `src/lib/api.ts` — the typed request layer, including the `X-API-Key` header
  and the 401 handling.
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
