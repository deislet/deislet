# Control HTTP API

The HTTP API of `deis-control`: what the `deis` CLI, the web console and any
other client call. It has two live namespaces - the product API under
`/api/v1` and the operator diagnostics under `/api/platform` - plus the flat
`/api/*` tree they grew out of, which is still served for a deprecation
window.

**Authority.** The machine-readable contract is
[`contracts/openapi/v1.json`](../../contracts/openapi/v1.json), generated
from the handlers themselves and held to them by a drift test
(`cargo test -p deis-control --test openapi_drift`) and a compatibility gate
(`python3 scripts/check-openapi-compat.py`). The examples in this document
are captured from real handler answers into
[`contracts/openapi/v1-examples.json`](../../contracts/openapi/v1-examples.json);
identifiers, instants, digests and cursors in them are fixed placeholders.
The error envelope and the list envelope have their own shared schemas in
[`contracts/v1/`](../../contracts/v1/). The design decisions behind all of
this are recorded in
[`docs/adr/016-api-cli-web-console.md`](../../docs/adr/016-api-cli-web-console.md).
Where this document and the OpenAPI document disagree, the OpenAPI document
is right and this one is a bug.

**Base URL.** Whatever `deis-control` binds to; `http://localhost:8080` in a
default single-box installation. Every path below is absolute from there.

---

## Table of Contents

- [Surfaces, versions and the deprecation window](#surfaces-versions-and-the-deprecation-window)
- [Request context: `x-request-id` and `x-correlation-id`](#request-context-x-request-id-and-x-correlation-id)
- [Authentication](#authentication)
- [Authorization: scopes and roles](#authorization-scopes-and-roles)
- [Errors](#errors)
- [Lists: `limit`, `cursor` and `page`](#lists-limit-cursor-and-page)
- [Idempotency](#idempotency)
- [`ETag` and `If-Match`](#etag-and-if-match)
- [Long operations](#long-operations)
- [Product API - `/api/v1`](#product-api---apiv1)
- [Platform diagnostics - `/api/platform`](#platform-diagnostics---apiplatform)
- [Permanent exemptions](#permanent-exemptions)
- [Node-facing routes](#node-facing-routes)
- [There is no WebSocket API](#there-is-no-websocket-api)
- [Generating a client](#generating-a-client)

---

## Surfaces, versions and the deprecation window

Three trees are served, and a route belongs to exactly one of them:

| Surface | Prefix | What is there |
|---|---|---|
| Product | `/api/v1` | Applications and everything inside them, accounts, tokens, the audit log, hostname grants, providers, backend restores, long operations. |
| Platform | `/api/platform` | Operator diagnostics: control channels, node configuration and node commands, events, the metrics summary, the runtime fleet, the global route table. |
| Legacy | `/api` (flat) | Every route above under its old spelling, deprecated. Plus the permanent exemptions, which are not deprecated and never move. |

The major version is spelled in the path, once, as `v1`. There is no version
header and no content negotiation: a client that wants v1 asks for `/api/v1`.
Within `v1`, changes are additive - a new field on an answer, a new optional
parameter, a new route - and the compatibility gate refuses anything else
without a major bump.

The split between product and platform is about *who* a route is for, not
about how it is implemented. A route under `/api/platform` answers about the
installation as a whole (which machines are up, what one node is running,
what the event log says); a route under `/api/v1` answers about an
application. The same handler serves the legacy spelling of both.

### The legacy tree and its deprecation

Every legacy path still answers exactly as it did, including the flat error
body (see [Errors](#errors)). What changed is that a matched legacy answer
now carries three headers:

```http
Deprecation: @1788652800
Sunset: Wed, 30 Jun 2027 00:00:00 GMT
Link: </api/v1/apps/demo?limit=2>; rel="successor-version"
```

- `Deprecation` is the instant the legacy spelling was declared deprecated,
  2026-09-06T00:00:00Z, in the `@<unix seconds>` form of RFC 9745.
- `Sunset` is when it is due to stop answering, 2027-06-30T00:00:00Z, in the
  HTTP-date form of RFC 8594.
- `Link` names the successor of *this* request - same path, same query
  string, under `/api/v1` or `/api/platform` - so a client can follow it
  without knowing the mapping.

The control plane also logs the deprecated route once per process, at `WARN`,
naming the template rather than the concrete path, so a busy fleet produces
one line per route instead of one per call.

`GET /api/metrics` is excluded: it is a permanent exemption, carries no
deprecation headers, and is not going to move. So are the node-facing routes
- see [Permanent exemptions](#permanent-exemptions).

### Moving a client

1. Prefix product calls with `/api/v1` and platform calls with
   `/api/platform`. Nothing else about the path changes.
2. Read errors from the structured envelope rather than from the message
   string.
3. Read lists from the `ControlListPage` envelope (`items` + `page`).
4. Keep `GET /api/metrics` exactly where it is.

The `deis` CLI already does this: it sends every product call to `/api/v1`
and every fleet call to `/api/platform`, and falls back to the flat tree
once, with a warning, when a control plane older than the CLI answers a
versioned path with a 404 that is not the structured envelope.

---

## Request context: `x-request-id` and `x-correlation-id`

Every answer served by one of the three trees carries two headers:

```http
x-request-id: 018f2518-7e38-7f80-8000-000000000001
x-correlation-id: 018f2518-7e38-7f80-8000-000000000001
```

- `x-request-id` identifies this one request. It is also the `requestId`
  inside a structured error body, and the `requestId` column of the audit
  line the call produced - so an error a user pasted can be found in the log
  without guessing.
- `x-correlation-id` identifies a chain of requests. It defaults to the
  request id when the caller sends none.

A caller may choose either id by sending the header, but only in the exact
form the control plane uses: **one** header occurrence carrying **one**
canonical lowercase UUIDv7. Anything else - a UUIDv4, an uppercase spelling,
two occurrences, free text - is discarded and a fresh id is minted, and the
minted id is what the handlers see and what comes back. Nothing a caller
sends is ever echoed unchecked. This holds on the legacy tree too: the long
operations under `/api/migrations`, `/api/restores` and the provider catalog
used to parse the inbound header themselves and answer `400` to a malformed
one, and now proceed under a minted id like every other route.

Three answers are produced outside the three trees and settle the id
themselves: the authorization gate's 401 and 403 and the 404 for an `/api`
path that matches no route (see [Errors](#errors)). They carry
`x-request-id` - the caller's own when it sent a usable one, a minted one
otherwise, and the same id the audit line of a refusal records - but no
`x-correlation-id`. The node-facing routes have no request context at all.

---

## Authentication

Two equivalent ways to present a credential:

```bash
curl -H "Authorization: Bearer <token>" http://localhost:8080/api/v1/apps
curl -H "X-API-Key: <token>"            http://localhost:8080/api/v1/apps
```

A token is either one minted through `POST /api/v1/accounts/{account}/tokens`
(`deis token create`), which belongs to an account and carries scopes, or an
`api_keys` entry of the configuration file, which is checked against the file
and not against the database.

```toml
# Require a credential from everybody.
api_auth_enabled = true

# Static keys, checked against this file. Prefer minted tokens: these have no
# account, no scopes and no revocation short of an edit and a restart.
api_keys = ["your-secret-api-key-1"]
```

With `api_auth_enabled = true` the control plane prints one owner token at
startup; every other credential is minted with it.

### What `api_auth_enabled = false` means

It means no credential is *required*. It has never meant that a credential
which *is* presented goes unchecked, and it does not publish the API to the
network. With it off an unauthenticated caller is admitted only when all
three hold:

- the request came over a direct connection from the machine the control
  plane runs on;
- it carries no proxy forwarding headers - a reverse proxy's TCP connection
  is local even when its caller is not;
- it is not a web page on another origin. A browser on the operator's own box
  is a loopback peer, and every page it has open would otherwise be one.

A credential that is presented is held to its scopes either way, and every
change is recorded in the audit log - as `anonymous` when nobody was
authenticated.

### Public endpoints

`GET /health` is the only route in front of the authorization gate. It
answers the constant `OK`, so a prober learns nothing a TCP connect had not
already told it, and the load balancer or orchestrator asking has no
credential to be given.

`GET /api/metrics` is **not** public: it hands out the cluster size, the node
count, the uptime and the event totals, so it goes through the gate like
every other read and a scrape from another host carries a token
(`deis token create --account prometheus --scope 'read:*/*' scrape`).

### Cross-origin access

A page from another origin can read these answers only when its origin is
named in `api_cors_allowed_origins`; `*` is refused there. The console this
binary serves is same-origin and needs no entry, and `npm run dev` proxies
`/api` through the vite server so the development console is same-origin too.
Being named is not authority: a page from another origin is refused the
unauthenticated local exception whichever machine its browser is on, and
presents a token instead.

### Handling the credential

1. Generate strong keys (`openssl rand -hex 32`) and keep them out of version
   control.
2. Prefer minted, scoped, revocable tokens over `api_keys`.
3. Terminate TLS in front of this process in production.
4. Rotate: `deis token create` for the new one, `DELETE /api/v1/tokens/{id}`
   for the old.

---

## Authorization: scopes and roles

One gate in front of the whole human API decides every call from one table,
which is a pure function of the method, the path and (for one route) the
query string. A route with no line in that table is reachable only by
`admin:*/*` and is audited every time: the failure mode of a forgotten route
is a refusal, not a hole.

### The scope grammar

```text
<verb>:<application>/<environment>

read:*/*                  look at everything, change nothing
deploy:myapp/preview      ship code into one environment of one application
secrets:myapp/production  write and delete that environment's secrets
storage:myapp/production  mint the credential that acts as that tenant
admin:*/*                 accounts, tokens, the audit log, nodes, deletions
```

`*` stands for a whole component and never for part of one: there is no
`my*`, and no `myapp/pre*`.

| Verb | What it covers |
|---|---|
| `read` | Lists and single reads, including the *names* of secrets. No endpoint returns a secret value to this API. |
| `deploy` | Upload a version, create an environment, deploy, roll back, change routes, unload an application from a node. |
| `secrets` | Write and delete an environment's secret values. Separate from `deploy` so a build pipeline cannot rotate what it deploys against. |
| `storage` | Mint the deis-store capability token for one tenant. Implied by nothing, and must name one application and one environment - `storage:*/*` is refused. |
| `admin` | The platform rather than an application. Only ever written `admin:*/*`; anything narrower is refused as meaningless. |

Every verb implies `read` on the same resource, and nothing else is implied
in either direction: `admin` does not answer `deploy`, and `deploy` does not
answer `storage`.

### Roles

A token can never carry more than its account's role allows.

| Role | Verbs it may hold |
|---|---|
| `owner` | all five, including `admin` |
| `maintainer` | `read`, `deploy`, `secrets`, `storage` |
| `deployer` | `read`, `deploy` |
| `viewer` | `read` |

### What each family of routes demands

`<app>` and `<env>` mean the names in the request path; a scope satisfies
them with the same name or with `*`.

| Routes | Demand |
|---|---|
| `GET /api/v1/apps` | `read:*/*` - it names every application |
| `POST /api/v1/apps` | `deploy:<app named in the body>/<any>` |
| `GET /api/v1/apps/{app_id}`, artifacts, versions (reads) | `read:<app>/<any>` |
| Artifact and version uploads | `deploy:<app>/<any>` - a bundle is stored, not run |
| `POST /api/v1/apps/{app_id}/environments` | `deploy:<app>/<env named in the body>` |
| Environment, deployment, route and secret-name reads | `read:<app>/<env>` |
| Deploy, roll back, create/update/delete a route | `deploy:<app>/<env>` |
| Write or delete a secret | `secrets:<app>/<env>` |
| Mint a storage token | `storage:<app>/<env>`, exactly named |
| Read a provider deployment or rollback operation through `GET /api/v1/operations/{operation_id}` | `read:<app>/<env>` - read from the operation's record, as the scoped `getProviderDeployment` demands |
| Cancel a provider deployment or rollback, through its scoped route or through `POST /api/v1/operations/{operation_id}/cancel` | `deploy:<app>/<env>` - for the generic route, read from the operation's record |
| Accounts, tokens, audit, hostname grants, providers, every other operation read and cancel, identity migration, backend restores, retired apps, `DELETE /api/v1/apps/{app_id}`, `GET /api/v1/deployment-plans/{id}` | `admin:*/*` |
| Platform: sessions, events, metrics, node configuration and commands, `GET /api/platform/routes`, `GET /api/platform/runtime/{node}/status` | `read:*/*`, and `admin:*/*` for the writes (configuration, commands) |
| `GET /api/platform/runtime` | any scope at all - it is the fleet's health, which `deis status` prints |
| `DELETE /api/platform/runtime/{node}/apps/{app_id}` | `deploy:<app>/<env>` with `?environment=<env>`; without it the call takes *every* environment off the node, so it demands `deploy:<app>/*` |

Two refinements are worth knowing about because they change what a narrow
token can do:

- **The body names the resource on four routes.** Creating an application or
  an environment names it in the body rather than in the path, so the gate
  reads those two bodies and narrows the demand to the name it finds - which
  is what lets a `deploy:myapp/preview` credential run `deis deploy` end to
  end. The two route writes are read for a different reason: their body names
  a *hostname*, no scope can name one, and what the gate takes from the body
  is the record for the audit line. Whether the application may claim that
  hostname is decided by the grant registry inside the same database call
  that writes the row.
- **Grants are platform statements.** A hostname grant, like an account or a
  token, names an application that is deliberately not the caller's, so
  `admin:*/*` and nothing narrower.

Product routes are refused under `/api/platform` and platform routes under
`/api/v1`: a route spelled under the wrong prefix falls to the unclassified
arm, which demands `admin:*/*` and is audited.

### The audit log

Every call through the gate produces a line - the account, the action, the
method and path as spelled, the target, the outcome, and the `requestId` of
the answer. Read it with `GET /api/v1/audit`.

---

## Errors

### Two shapes, and which surface has which

On `/api/v1` and `/api/platform` an error is one JSON object under `error`:

```json
{
  "error": {
    "code": "not-found",
    "message": "app no-such-app not found",
    "requestId": "018f2518-7e38-7f80-8000-000000000001",
    "retryable": false,
    "violations": []
  }
}
```

| Member | Always present | Meaning |
|---|---|---|
| `code` | yes | The stable class. Branch on this; never on `message`. |
| `message` | yes | Human-readable and not stable between releases. On `internal`, `database-unavailable` and a `backend-unavailable` raised by a component the control plane dialled, it is a fixed sentence: what the component said (an SQLite error, an io path, a node's address) goes to the server log under `requestId`, never into the answer. |
| `requestId` | yes | The same id as the `x-request-id` header of this answer. |
| `retryable` | yes | Whether the identical request, unchanged, may succeed later. A property of the code. |
| `violations` | yes (may be `[]`) | Field-level findings: `field`, `code`, `message`. |
| `target` | no | `{"resource": ..., "field": ...}` when one thing can be named. |
| `retryAfterSeconds` | no | Only ever on a retryable code. |
| `docs` | no | Where the rule that refused the request is written down; an absolute HTTPS link. |
| `provider` | no | A provider driver's own diagnostic in the driver's closed vocabulary - `backendId`, `category`, `code`, `sideEffect`, `requestId` - and nothing else. |

The schema is
[`contracts/v1/control-error.schema.json`](../../contracts/v1/control-error.schema.json);
the code list there, the enum in the OpenAPI document and the server's own
table are pinned to each other by tests.

On the **legacy** tree the body is byte-for-byte what it always was, a flat
string:

```json
{"error": "app no-such-app not found"}
```

Nothing about the legacy body changed, including the `Database error: `
prefix it has always carried on a 500. Clients that grep messages keep
working there for the deprecation window; that is the point.

### The gate and the route fallback answer in the shape of the prefix

The authorization gate runs in front of all three trees, and the fallback for
an `/api` path that matches no route runs outside them; both read the prefix
off the path and answer in its shape. Under `/api/v1` and `/api/platform`
that is the envelope above, with `unauthenticated`, `insufficient-scope` or
`route-not-found` as the code and the `x-request-id` header set to the same
id as `requestId`. Under the flat `/api` tree it is the two-key body those
answers have always had, byte for byte:

```json
{"error": "Unauthorized",
 "details": "A valid credential is required. Present it as 'Authorization: Bearer <token>' or 'X-API-Key: <token>'. Mint one with `deis token create`."}
```

```json
{"error": "This credential may not do that: <what was refused>",
 "details": "A token carrying <scope> would be allowed. Ask an owner for one with `deis token create --account <you> --scope <scope>`."}
```

```json
{"error": "Not Found",
 "details": "No such endpoint. The API surface is listed in the component's API_DOCUMENTATION.md."}
```

These three carry `x-request-id` on every prefix and no deprecation headers.
A path spelled in a way the router did not match (`/API/v1/...`) is a legacy
path for this purpose.

### The code table

| Code | HTTP | Retryable | When |
|---|---|---|---|
| `invalid-argument` | 400 | no | Malformed body, query or path value; validation failed. |
| `limit-out-of-range` | 400 | no | A `limit` outside `1..=1000` on any list route. `target` is `{resource: "query", field: "limit"}`. |
| `offset-out-of-range` | 400 | no | Reserved, and emitted by nothing: offset paging was retired before any route shipped it. Kept in the closed enum because a published code is never withdrawn. |
| `cursor-invalid` | 400 | no | A cursor that was not issued for this collection and these filters, or has expired. `target` is `{resource: "query", field: "cursor"}`. |
| `idempotency-key-required` | 400 | no | A mutation that replays under a key was sent without one. `target` is `{resource: "request", field: "Idempotency-Key"}`. |
| `unauthenticated` | 401 | no | Nothing usable was presented. From the gate, see above. |
| `insufficient-scope` | 403 | no | Authenticated, but the scope does not cover this call. From the gate, see above. |
| `forbidden` | 403 | no | The call is refused for a reason other than scope. |
| `not-found` | 404 | no | No such application, environment, version, record. |
| `route-not-found` | 404 | no | No such endpoint. From the route fallback, see above. |
| `already-exists` | 409 | no | A create refused because the name is taken: an application (including one whose deleted namesake still holds data), an environment, a hostname grant, an account. `target.resource` is the kind. |
| `conflict` | 409 | no | The request contradicts the current state. |
| `not-configured` | 409 | no | A dependency this call needs is not configured (no master key, no store endpoint). The message names the setting. |
| `idempotency-key-reused` | 409 | no | The key belongs to an earlier request with different parameters. |
| `idempotency-request-in-progress` | 409 | **yes** | The first request under this key has not finished. Retry the identical request. |
| `provider-installation-has-dependents` | 409 | no | Targets still hang off the installation. |
| `provider-installation-in-use` | 409 | no | The installation still backs an active grant or deployment. |
| `provider-target-in-use` | 409 | no | The target still backs an active grant. |
| `provider-resource-deleted` | 409 | no | The provider record was soft-deleted. |
| `operation-not-cancellable` | 409 | no | The operation is terminal or already cancelling. |
| `operation-not-rollbackable` | 409 | no | The operation is not a succeeded deployment on this environment, or its target grant is no longer the active one. |
| `revision-conflict` | 412 | no | The `If-Match` revision is not the current one, or another writer moved it first. |
| `payload-too-large` | 413 | no | The body is larger than the route reads. |
| `precondition-required` | 428 | no | The mutation needs an `If-Match` and none was sent. |
| `internal` | 500 | no | This process failed on its own side. |
| `database-unavailable` | 500 | **yes** | Its database did. |
| `backend-unavailable` | 502 or 503 | **yes** | A node, provider driver or storage backend it had to reach did. |
| `service-unavailable` | 503 | **yes** | A dependency is unavailable. |

`retryable` follows the code and nothing else: it is `true` for exactly the
four codes marked above, on every answer that carries one of them, and
`false` on every other. `retryAfterSeconds` only ever appears beside
`retryable: true`; it advises how long to wait, and its absence means the
client picks its own backoff. Everything not retryable is a defect in the
request: retrying it unchanged produces the same answer.

One code is marked *reserved*: `offset-out-of-range`. Nothing emits it and
nothing will - every list is keyset-paged behind an opaque cursor and no
route takes an offset - but it stays in the closed enum, because a code that
has been published is never withdrawn from it and a client that already
branches on it must keep compiling.

A refusal that decomposes into named members carries them in `violations`,
each `{field, code, message}` with the field in dotted path form
(`scopes[0]`, `config.zoneId`). Where the route can attribute a validation
failure to exactly one member of the body it does - `id` on an application
create, `name` on an environment or account create, `role` and `scopes[n]`
on the identity routes - and where a single sentence covers several members
it stays a `target`-less `invalid-argument` rather than guessing.

---

## Lists: `limit`, `cursor` and `page`

Every list route on both surfaces answers in one envelope, whatever it lists:

```json
{
  "schemaVersion": 1,
  "items": [ "..." ],
  "page": { "limit": 100, "count": 1, "total": 1, "nextCursor": null }
}
```

| Member | Meaning |
|---|---|
| `schemaVersion` | Always `1` in this document. |
| `items` | This page, in the collection's own order (every diagnostic list is newest first). |
| `page.limit` | The page size that was applied. |
| `page.count` | How many items this answer carries. |
| `page.total` | How many items match in total, counted in the same database transaction as the items - so the count and the page describe one instant. |
| `page.nextCursor` | Where the next page starts, or `null` on the last page. |

The parameters are the same everywhere:

- `limit` - `1..=1000`; absent means 100. Outside that range is a 400.
- `cursor` - the `page.nextCursor` of the previous answer, verbatim.

A cursor is **opaque**: a signed token bound to the collection, to the
filters that were in force when it was issued, and to the sort. Do not parse
it, do not build one, and do not carry one from one query to another - a
cursor issued for one node's command history is refused on another node's,
and a cursor issued without a filter is refused once a filter is applied. A
foreign cursor is a 400.

Paging is keyset-based, not offset-based: rows inserted while a client walks
the pages do not shift the ones it has not read yet, and there is no offset
parameter to grow expensive.

```bash
page=$(curl -s -H "Authorization: Bearer $TOKEN" \
  "http://localhost:8080/api/v1/apps?limit=100")
next=$(printf '%s' "$page" | jq -r '.page.nextCursor')
[ "$next" != "null" ] && curl -s -H "Authorization: Bearer $TOKEN" \
  "http://localhost:8080/api/v1/apps?limit=100&cursor=$next"
```

The schema is
[`contracts/v1/control-list-page.schema.json`](../../contracts/v1/control-list-page.schema.json).

---

## Idempotency

Some mutations are expensive, external, or both: sealing a provider
credential, installing a driver release, starting an identity migration or a
backend restore. Those **require** an `Idempotency-Key` request header, and
answer `428` with `precondition-required` without one.

```http
POST /api/v1/providers/credentials
Idempotency-Key: 7f4c1a2e-provision-shop-prod
```

Rules:

- The key is 8 to 255 bytes with no control characters. Anything else is a
  400. A UUID is the obvious choice; anything unique to the intent works.
- The key is scoped to the organization, the actor and the route. Two
  different callers may use the same string without colliding, and the same
  key on a different route is a different key.
- **Replay**: the same key with the same parameters returns the first
  request's answer, including its status. Retrying after a timeout is safe
  and is the intended use.
- **Conflict**: the same key with *different* parameters is a 409
  `idempotency-key-reused`. Change the key when the request changes.
- **In flight**: the same key while the first request is still running is a
  409 `idempotency-request-in-progress`, which is retryable - wait and send
  the identical request again.

The routes that require a key are listed with it in the reference below:
every provider credential, driver-release, installation, target and
target-grant mutation, `POST /api/v1/apps/{application_name}/identity-migration`,
`POST /api/v1/backend-restores`, `POST /api/v1/operations/{operation_id}/cancel`
and `POST` on the app-scoped provider deployments. No other route reads the
header today; sending one there is harmless and ignored.

Deploy and rollback are not on that list. They are made safe by the
deployment plan instead: a repeated deploy of the same version reuses the
plan it finds rather than starting a second one.

---

## `ETag` and `If-Match`

Records that can be changed by more than one caller carry a revision. It
grows with every change and is returned as an `ETag`:

```http
ETag: "7"
```

The mutations on those records take the revision back in `If-Match`, as one
strong, quoted value:

```http
If-Match: "7"
```

- Missing where it is required: `428` `precondition-required`.
- Not the current revision: `412` `revision-conflict` - somebody else changed
  the record; re-read it, decide again, resend.
- Not one strong quoted number: `400`.

`If-Match` is required on every provider record mutation (disable, enable,
delete, revoke, rotate), on starting an identity migration, and on cancelling
an operation. Which ones is recorded per operation in the reference below,
and in the OpenAPI document.

Two notes on the edges of this:

- Some records carry a `revision` field in their body as well as in the
  `ETag`; they are the same number.
- The secret routes take no `If-Match`: the compare-and-set is against the
  revision the control plane itself read in the same call. A `412` there
  means another writer moved the secret between that read and the write, and
  the fix is the same - re-read and resend.

---

## Long operations

A mutation that cannot finish inside the request answers `202 Accepted` with
a `Location` header and a record to poll. There are two kinds, and they are
not interchangeable.

**Product operations** - identity migration, backend restore, provider
deployment, operation cancellation. The answer is a `ProductOperation`
(schema:
[`contracts/v1/operation.schema.json`](../../contracts/v1/operation.schema.json)),
`Location` points at `/api/v1/operations/{operation_id}` (at the scoped
provider deployment for a provider deploy or rollback), and the record
carries a `state`, a `revision` (its `ETag`), `progress`, whether it is
`cancellable`, and an `error` that is set exactly when the state is `failed`.
Cancel with `POST /api/v1/operations/{operation_id}/cancel`, which needs an
`If-Match` and takes an `Idempotency-Key` as optional, exactly as the scoped
provider-deployment cancel does; it dispatches on the operation's kind and
demands the scope the record says the operation belongs to. Reading an
operation demands the same record's scope: `read:<app>/<env>` for a
provider deploy or rollback, `admin:*/*` for everything else.

**Deployment plans** - deploy, rollback, and a secret write or delete that
has to re-plan the running deployment. The answer is the normal deployment
body, and `Location` points at the plan:
`/api/v1/apps/{app_id}/environments/{env_name}/deployment-plans/{id}` for an
application credential, or the cluster-wide `/api/v1/deployment-plans/{id}`
for an administrator. Poll until the plan is no longer draining.

`Location` is always spelled for the surface the call came in on: a mutation
sent to `/api/v1` is pointed back at `/api/v1`, one sent to the legacy tree
at the legacy tree. Follow the header rather than rebuilding the path.

---

## Product API - `/api/v1`

Everything in this chapter is `product`-tagged in the OpenAPI document,
served under `/api/v1`, and also - for the deprecation window - under the
flat `/api` spelling named with each operation.

The refusals listed per operation are the ones the document declares. `401`
and `403` can be answered on any route by the authorization gate, in the
envelope under `/api/v1` and `/api/platform` and in the two-key shape on the
legacy tree.

### Applications

An application is the unit of naming on this platform: the `app_id` every
other component keys off. Deleting one takes its versions, environments,
deployment history, routes and secrets with it and tells every node to stop
serving it, so it answers to `admin:*/*` rather than to whoever may deploy
into one of its environments. The name it frees is kept in the retired list
until an administrator releases it, because the next application of that name
inherits the deleted one's stored data.

#### `GET /api/v1/apps`

`operationId: listApps`. Legacy spelling `GET /api/apps`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_App` - One page of applications.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_App`):

```json
{
  "items": [
    {
      "canonical_id": "018f2518-7e38-7f80-8000-000000000001",
      "created_at": 1767225600,
      "id": "demo",
      "revision": 1
    }
  ],
  "page": {
    "count": 1,
    "limit": 100,
    "nextCursor": null,
    "total": 1
  },
  "schemaVersion": 1
}
```

#### `POST /api/v1/apps`

`operationId: createApp`. Legacy spelling `POST /api/apps`, deprecated.

- **Body** (`application/json`, required) `CreateAppRequest`
  - `id` (string, required) - App name; also the app_id every component keys off.
- **201** `App` - The application as created.
- **Refusals** `400`, `401`, `403`, `409` - see [Errors](#errors).

Example answer (`201`, `App`):

```json
{
  "canonical_id": "018f2518-7e38-7f80-8000-000000000001",
  "created_at": 1767225600,
  "id": "demo",
  "revision": 1
}
```

#### `GET /api/v1/apps/{app_id}`

`operationId: getApp`. Legacy spelling `GET /api/apps/{app_id}`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **200** `App` - The application.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

Example answer (`404`, `ErrorBody`):

```json
{
  "error": {
    "code": "not-found",
    "message": "app no-such-app not found",
    "requestId": "018f2518-7e38-7f80-8000-000000000001",
    "retryable": false,
    "violations": []
  }
}
```

#### `DELETE /api/v1/apps/{app_id}`

Removes the app together with its versions, environments, deployment history, routes and secrets, drops the bundle files those versions referenced, and tells every node to stop serving it.

That last step is not optional. A node adds applications from its assignment list and never removes one that has left it, so a delete that only touched the database would leave every node serving the application forever: still answering requests, still holding its decrypted secrets in memory, still polling its queues. A delete done to revoke access has to actually revoke it, so a node that cannot be reached is reported in the answer rather than passed over.

What it does *not* remove is the application's stored data. That lives in deis-store and deis-queue, keyed by `(application, environment)`, and this control plane has no call that drops a tenant there, so the answer reports the tenants that still hold it and the name is retained so that the next application of that name cannot be handed it silently.

`operationId: deleteApp`. Legacy spelling `DELETE /api/apps/{app_id}`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **200** `DeleteAppResponse` - What was removed, what was unloaded, and the name now retired.
- **Refusals** `401`, `403`, `404`, `409`, `503` - see [Errors](#errors).

Example answer (`200`, `DeleteAppResponse`):

```json
{
  "app_id": "demo",
  "artifact_directories_removed": 0,
  "bundle_files_removed": 1,
  "removed": {
    "canonical_artifacts": 0,
    "deployments": 1,
    "environments": 1,
    "provider_derivations": 0,
    "routes": 0,
    "secrets": 0,
    "versions": 1
  },
  "retained": {
    "app_id": "demo",
    "environments": [
      "production"
    ],
    "released_at": null,
    "retired_at": 1767225600
  },
  "retained_note": "the kv, database, object and queue data of demo is still in deis-store and deis-queue under tenant(s) demo.production. No part of this platform deletes it. The name is retained until it is released with DELETE /api/retired-apps/demo, so nothing can be deployed onto that data by accident",
  "unloaded": {
    "app_id": "demo",
    "environments": [
      "production"
    ],
    "failed": [],
    "skipped": null,
    "unloaded": [
      "runtime-test"
    ]
  }
}
```

#### `GET /api/v1/retired-apps`

The names of deleted applications and the tenants whose data is the reason they are held. This is the only record of what became of a deleted application's data: its catalog rows are gone, and deis-store and deis-queue cannot be asked what they still hold.

A row with `released_at` set is a name somebody freed again: it can be created, and what it says about the data is as true as it ever was.

`operationId: listRetiredApps`. Legacy spelling `GET /api/retired-apps`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_RetiredAppName` - One page of retired names.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_RetiredAppName`):

```json
{
  "items": [],
  "page": {
    "count": 0,
    "limit": 100,
    "nextCursor": null,
    "total": 0
  },
  "schemaVersion": 1
}
```

#### `DELETE /api/v1/retired-apps/{app_id}`

Free the name again. Deliberately a second call, made by an administrator, rather than something the delete could have done on its own: releasing it is the decision that the next application created under this name may have whatever the deleted one left in deis-store and deis-queue. Whoever makes it is in the audit log, and so is this call when it is refused.

The data itself is untouched either way. An operator who wants it gone purges it - `deis kv`, `deis db`, `deis blob` against those tenants while the application still exists, or the tenant's files under deis-store's own data directory - and then releases the name.

`operationId: releaseRetiredApp`. Legacy spelling `DELETE /api/retired-apps/{app_id}`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **200** `ReleaseRetiredAppResponse` - The name is free again.
- **Refusals** `401`, `403`, `404`, `409` - see [Errors](#errors).

Example answer (`200`, `ReleaseRetiredAppResponse`):

```json
{
  "accepted": "demo is free again. the kv, database, object and queue data of demo is still in deis-store and deis-queue under tenant(s) demo.production, and the next application created under this name is that same tenant: it will read and overwrite whatever is there. Credentials follow the name too - a token scoped to this application, or a storage capability token minted for one of those tenants, still authorises the next one, so revoke what the deleted application's owner still holds",
  "app_id": "demo",
  "released": {
    "app_id": "demo",
    "environments": [
      "production"
    ],
    "released_at": 1767225600,
    "retired_at": 1767225600
  }
}
```

### Artifacts and versions

A version is a bundle uploaded to the application, not to an environment: it
is stored and put nowhere until a deployment points at it. A canonical
artifact is the content-addressed identity a version can be given so the same
build is recognisable across applications and installations.

#### `GET /api/v1/apps/{app_id}/artifacts`

`operationId: listArtifacts`. Legacy spelling `GET /api/apps/{app_id}/artifacts`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_CanonicalArtifact` - One page of canonical artifact records.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `POST /api/v1/apps/{app_id}/artifacts`

The request body is only a transport zip. Control expands it into an isolated directory and executes the Edge Canon validator before either a storage path or catalog record becomes visible.

`operationId: uploadArtifact`. Legacy spelling `POST /api/apps/{app_id}/artifacts`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Query** `sha256` (string, optional) - Expected identity of `.edge-canon/output.json`, not the transport zip.
- **Body** (`application/octet-stream`, required) `BinaryBody`
- **201** `ArtifactUploadResponse` - The artifact was verified and stored; `created` is false when it was already there.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `413`, `422` - see [Errors](#errors).

#### `GET /api/v1/apps/{app_id}/artifacts/{artifact_id}`

`operationId: getArtifact`. Legacy spelling `GET /api/apps/{app_id}/artifacts/{artifact_id}`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `artifact_id` (string, required) - The canonical artifact's UUIDv7.
- **200** `CanonicalArtifact` - The canonical artifact record.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

#### `POST /api/v1/apps/{app_id}/artifacts/{artifact_id}/versions`

The envelope contains exactly an immutable provider derivation record and the Deislet native bundle it binds. Control revalidates the stored canonical parent, the derivation, the payload root and the native bundle before creating one lineage-bound version row.

`operationId: uploadVersionWithLineage`. Legacy spelling `POST /api/apps/{app_id}/artifacts/{artifact_id}/versions`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `artifact_id` (string, required) - The canonical artifact's UUIDv7.
- **Query** `sha256` (string, optional) - Digest the client computed over the bundle. When present the bytes must hash to exactly this, which is how a truncated upload is caught before it can become a version.
- **Body** (`application/octet-stream`, required) `BinaryBody`
- **201** `UploadResponse` - The version was stored with its lineage to the canonical artifact.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `413`, `422` - see [Errors](#errors).

#### `GET /api/v1/apps/{app_id}/versions`

`operationId: listVersions`. Legacy spelling `GET /api/apps/{app_id}/versions`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_VersionView` - One page of versions.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_VersionView`):

```json
{
  "items": [
    {
      "app_id": "demo",
      "bundle_sha": "0000000000000000000000000000000000000000000000000000000000000000",
      "canonical_id": "018f2518-7e38-7f80-8000-000000000001",
      "compatibility": {},
      "created_at": 1767225600,
      "id": "018f2518-7e38-7f80-8000-000000000001",
      "manifest": {
        "app": "demo",
        "entry": "worker/index.js",
        "schema": 1,
        "secrets": [
          "API_KEY"
        ],
        "services": {
          "kv": true
        }
      },
      "number": 1
    }
  ],
  "page": {
    "count": 1,
    "limit": 100,
    "nextCursor": null,
    "total": 1
  },
  "schemaVersion": 1
}
```

#### `POST /api/v1/apps/{app_id}/versions`

Body is the raw bundle zip. The upload is rejected outright - nothing written, no row created - when the digest does not match, the manifest does not parse, or the manifest names a different app.

`operationId: uploadVersion`. Legacy spelling `POST /api/apps/{app_id}/versions`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Query** `sha256` (string, optional) - Digest the client computed over the bundle. When present the bytes must hash to exactly this, which is how a truncated upload is caught before it can become a version.
- **Body** (`application/octet-stream`, required) `BinaryBody`
- **201** `UploadResponse` - The version was stored; `created` is false when the same bundle was already there.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `413`, `422` - see [Errors](#errors).

Example answer (`201`, `UploadResponse`):

```json
{
  "bundle_url": "http://control.test/api/apps/demo/versions/018f2518-7e38-7f80-8000-000000000001/bundle",
  "created": true,
  "version": {
    "app_id": "demo",
    "bundle_sha": "0000000000000000000000000000000000000000000000000000000000000000",
    "canonical_id": "018f2518-7e38-7f80-8000-000000000001",
    "compatibility": {},
    "created_at": 1767225600,
    "id": "018f2518-7e38-7f80-8000-000000000001",
    "manifest": {
      "app": "demo",
      "entry": "worker/index.js",
      "schema": 1,
      "secrets": [
        "API_KEY"
      ],
      "services": {
        "kv": true
      }
    },
    "number": 1
  }
}
```

#### `GET /api/v1/apps/{app_id}/versions/{version_id}`

`operationId: getVersion`. Legacy spelling `GET /api/apps/{app_id}/versions/{version_id}`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `version_id` (string, required) - The version's id.
- **200** `VersionView` - The version.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `VersionView`):

```json
{
  "app_id": "demo",
  "bundle_sha": "0000000000000000000000000000000000000000000000000000000000000000",
  "canonical_id": "018f2518-7e38-7f80-8000-000000000001",
  "compatibility": {},
  "created_at": 1767225600,
  "id": "018f2518-7e38-7f80-8000-000000000001",
  "manifest": {
    "app": "demo",
    "entry": "worker/index.js",
    "schema": 1,
    "secrets": [
      "API_KEY"
    ],
    "services": {
      "kv": true
    }
  },
  "number": 1
}
```

### Environments

An environment is one running incarnation of an application - `production`,
`preview`, one per pull request - with its own version pointer, its own
variables and its own secrets. Its name, never its id, is what travels to the
runtime.

#### `GET /api/v1/apps/{app_id}/environments`

`operationId: listEnvironments`. Legacy spelling `GET /api/apps/{app_id}/environments`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_Environment` - One page of environments.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_Environment`):

```json
{
  "items": [
    {
      "app_id": "demo",
      "canonical_id": "018f2518-7e38-7f80-8000-000000000001",
      "created_at": 1767225600,
      "id": "018f2518-7e38-7f80-8000-000000000001",
      "name": "production"
    }
  ],
  "page": {
    "count": 1,
    "limit": 100,
    "nextCursor": null,
    "total": 1
  },
  "schemaVersion": 1
}
```

#### `POST /api/v1/apps/{app_id}/environments`

`operationId: createEnvironment`. Legacy spelling `POST /api/apps/{app_id}/environments`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Body** (`application/json`, required) `CreateEnvironmentRequest`
  - `name` (string, required) - `production` or any preview name.
- **201** `Environment` - The environment as created.
- **Refusals** `400`, `401`, `403`, `404`, `409` - see [Errors](#errors).

Example answer (`201`, `Environment`):

```json
{
  "app_id": "demo",
  "canonical_id": "018f2518-7e38-7f80-8000-000000000001",
  "created_at": 1767225600,
  "id": "018f2518-7e38-7f80-8000-000000000001",
  "name": "production"
}
```

#### `GET /api/v1/apps/{app_id}/environments/{env_name}`

`operationId: getEnvironment`. Legacy spelling `GET /api/apps/{app_id}/environments/{env_name}`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **200** `Environment` - The environment.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

### Deployments, rollbacks and plans

Deploying builds the exact payload, persists a plan and a snapshot, prepares
every frozen runtime and trigger, and commits the environment's pointer
before activating them. When the plan is still draining its pre-commit
barrier the answer is `202` with a `Location`; see
[Long operations](#long-operations). The pointer history is append-only, so
paging back through it is a stable walk backwards in time.

#### `GET /api/v1/apps/{app_id}/environments/{env_name}/deployment`

The current version of an environment: the newest pointer row.

`operationId: getCurrentDeployment`. Legacy spelling `GET /api/apps/{app_id}/environments/{env_name}/deployment`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **200** `DeploymentResponse` - The deployment the environment points at.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

#### `GET /api/v1/apps/{app_id}/environments/{env_name}/deployment-plans/{id}`

`operationId: getEnvironmentDeploymentPlan`. Legacy spelling `GET /api/apps/{app_id}/environments/{env_name}/deployment-plans/{id}`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **Path** `id` (string, required) - The plan's id.
- **200** `DeploymentPlanResponse` - The plan and what every node and proxy observed of it.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

#### `GET /api/v1/apps/{app_id}/environments/{env_name}/deployments`

The pointer history of one environment, newest first. Every row is a deployment or a rollback; nothing is ever rewritten, so paging through it is a stable walk backwards in time.

`operationId: listDeployments`. Legacy spelling `GET /api/apps/{app_id}/environments/{env_name}/deployments`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_Deployment` - One page of deployments, newest first.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_Deployment`):

```json
{
  "items": [
    {
      "app_id": "demo",
      "canonical_id": "018f2518-7e38-7f80-8000-000000000001",
      "created_at": 1767225600,
      "env_id": "018f2518-7e38-7f80-8000-000000000001",
      "id": "018f2518-7e38-7f80-8000-000000000001",
      "note": null,
      "version_id": "018f2518-7e38-7f80-8000-000000000001"
    }
  ],
  "page": {
    "count": 1,
    "limit": 100,
    "nextCursor": null,
    "total": 1
  },
  "schemaVersion": 1
}
```

#### `POST /api/v1/apps/{app_id}/environments/{env_name}/deployments`

Build the exact payload, persist a plan and snapshot, prepare every frozen runtime and trigger, then commit the pointer before activating them. An environment whose secrets cannot be decrypted is refused before the plan or pointer exists; a plan still draining its pre-commit barrier is returned as `202 Accepted` and remains queryable through `Location`.

`operationId: deploy`. Legacy spelling `POST /api/apps/{app_id}/environments/{env_name}/deployments`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **Body** (`application/json`, required) `DeployRequest`
  - `config_values` (object, optional) - Deployment-time config only. Secret values use the dedicated secret write API and are selected by immutable revision.
  - `note` (string/null, optional)
  - `version_id` (string/null, optional) - Target version by id. Takes precedence over `version_number`.
  - `version_number` (integer/null, optional) - Target version by its per-app number, i.e. the "v3" a human says.
- **201** `DeploymentResponse` - The deployment was recorded and delivered.
- **202** `DeploymentResponse` with `Location` - The deployment was planned and is being coordinated; poll `Location`.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `503` - see [Errors](#errors).

Example answer (`201`, `DeploymentResponse`):

```json
{
  "delivery": {
    "app_id": "demo",
    "delivered": [
      "runtime-test"
    ],
    "environment": "production",
    "failed": [],
    "phase": "activate",
    "reason": "deployment",
    "skipped": null,
    "version_id": "018f2518-7e38-7f80-8000-000000000001"
  },
  "deployment": {
    "app_id": "demo",
    "canonical_id": "018f2518-7e38-7f80-8000-000000000001",
    "created_at": 1767225600,
    "env_id": "018f2518-7e38-7f80-8000-000000000001",
    "id": "018f2518-7e38-7f80-8000-000000000001",
    "note": null,
    "version_id": "018f2518-7e38-7f80-8000-000000000001"
  },
  "environment": "production",
  "plan": {
    "app_id": "demo",
    "compare_secret_heads": true,
    "created_at": 1767225600,
    "env_id": "018f2518-7e38-7f80-8000-000000000001",
    "expected_secret_heads": {},
    "id": "018f2518-7e38-7f80-8000-000000000001",
    "last_error": "all frozen runtime and proxy targets report the exact production generation",
    "plan_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
    "policy": {
      "activation": "strict_all",
      "affinity": "stable",
      "drain": {
        "retentionSeconds": 300,
        "timeoutSeconds": 30
      },
      "operation": "deployment",
      "queueFence": false,
      "secretChange": null,
      "traffic": [
        {
          "basisPoints": 10000,
          "generation": "018f2518-7e38-7f80-8000-000000000001"
        }
      ]
    },
    "proxy_ids": [],
    "routing_generation": "018f2518-7e38-7f80-8000-000000000001",
    "state": "active",
    "target_ids": [
      "runtime-test"
    ],
    "updated_at": 1767225600,
    "version_id": "018f2518-7e38-7f80-8000-000000000001"
  },
  "version": {
    "app_id": "demo",
    "bundle_sha": "0000000000000000000000000000000000000000000000000000000000000000",
    "canonical_id": "018f2518-7e38-7f80-8000-000000000001",
    "compatibility": {},
    "created_at": 1767225600,
    "id": "018f2518-7e38-7f80-8000-000000000001",
    "manifest": {
      "app": "demo",
      "entry": "worker/index.js",
      "schema": 1,
      "secrets": [
        "API_KEY"
      ],
      "services": {
        "kv": true
      }
    },
    "number": 1
  }
}
```

#### `POST /api/v1/apps/{app_id}/environments/{env_name}/rollback`

Coordinate a new immutable deployment aimed at an older version and its historical binding snapshot. The current pointer is changed only after the same strict prepare barrier used by a forward deployment, so rollback remains reversible and never silently rebinds secrets by name.

`operationId: rollback`. Legacy spelling `POST /api/apps/{app_id}/environments/{env_name}/rollback`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **Body** (`application/json`, required) `RollbackRequest`
  - `version_timestamp` (string, required) - The `version_timestamp` of a history entry for this key.
- **201** `DeploymentResponse` - The rollback was recorded and delivered.
- **202** `DeploymentResponse` with `Location` - The rollback was planned and is being coordinated; poll `Location`.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `503` - see [Errors](#errors).

#### `GET /api/v1/deployment-plans/{id}`

`operationId: getDeploymentPlan`. Legacy spelling `GET /api/deployment-plans/{id}`, deprecated.

- **Path** `id` (string, required) - The plan's id.
- **200** `DeploymentPlanResponse` - The plan and what every node and proxy observed of it.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `DeploymentPlanResponse`):

```json
{
  "drain_observations": [],
  "events": [
    {
      "created_at": 1767225600,
      "detail": "plan persisted before prepare",
      "from_state": null,
      "plan_id": "018f2518-7e38-7f80-8000-000000000001",
      "sequence": 1,
      "to_state": "preparing"
    },
    {
      "created_at": 1767225600,
      "detail": "every frozen runtime target acknowledged the exact candidate generation",
      "from_state": "preparing",
      "plan_id": "018f2518-7e38-7f80-8000-000000000001",
      "sequence": 2,
      "to_state": "prepared"
    },
    {
      "created_at": 1767225600,
      "detail": "production pointer committed",
      "from_state": "prepared",
      "plan_id": "018f2518-7e38-7f80-8000-000000000001",
      "sequence": 3,
      "to_state": "activating"
    },
    {
      "created_at": 1767225600,
      "detail": "all frozen runtime and proxy targets report the exact production generation",
      "from_state": "activating",
      "plan_id": "018f2518-7e38-7f80-8000-000000000001",
      "sequence": 4,
      "to_state": "active"
    }
  ],
  "plan": {
    "app_id": "demo",
    "compare_secret_heads": true,
    "created_at": 1767225600,
    "env_id": "018f2518-7e38-7f80-8000-000000000001",
    "expected_secret_heads": {},
    "id": "018f2518-7e38-7f80-8000-000000000001",
    "last_error": "all frozen runtime and proxy targets report the exact production generation",
    "plan_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
    "policy": {
      "activation": "strict_all",
      "affinity": "stable",
      "drain": {
        "retentionSeconds": 300,
        "timeoutSeconds": 30
      },
      "operation": "deployment",
      "queueFence": false,
      "secretChange": null,
      "traffic": [
        {
          "basisPoints": 10000,
          "generation": "018f2518-7e38-7f80-8000-000000000001"
        }
      ]
    },
    "proxy_ids": [],
    "routing_generation": "018f2518-7e38-7f80-8000-000000000001",
    "state": "active",
    "target_ids": [
      "runtime-test"
    ],
    "updated_at": 1767225600,
    "version_id": "018f2518-7e38-7f80-8000-000000000001"
  },
  "proxies": [],
  "targets": [
    {
      "message": null,
      "node_id": "runtime-test",
      "observed_at": 1767225600,
      "observed_state": "active",
      "plan_id": "018f2518-7e38-7f80-8000-000000000001",
      "routing_generation": "018f2518-7e38-7f80-8000-000000000001",
      "version_id": "018f2518-7e38-7f80-8000-000000000001"
    }
  ]
}
```

### Routes

A route sends a hostname and path prefix to one environment. Which
application may claim a hostname is not decided here: it is decided by the
grant registry (see [Hostname grants](#hostname-grants)) inside the same
database call that writes the row, so a `deploy:<app>/<env>` credential can
create routes only for names its application has been granted.

#### `GET /api/v1/apps/{app_id}/environments/{env_name}/routes`

One environment's routes in the order the matcher applies them: highest priority first, then by path prefix, with the route id as the tie-breaker so paging across two routes on one prefix is stable.

`operationId: listRoutes`. Legacy spelling `GET /api/apps/{app_id}/environments/{env_name}/routes`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_Route` - One page of routes.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_Route`):

```json
{
  "items": [
    {
      "app_id": "demo",
      "env_id": "018f2518-7e38-7f80-8000-000000000001",
      "host": "demo.example.com",
      "id": "018f2518-7e38-7f80-8000-000000000001",
      "path_prefix": "/",
      "priority": 0
    }
  ],
  "page": {
    "count": 1,
    "limit": 100,
    "nextCursor": null,
    "total": 1
  },
  "schemaVersion": 1
}
```

#### `POST /api/v1/apps/{app_id}/environments/{env_name}/routes`

`operationId: createRoute`. Legacy spelling `POST /api/apps/{app_id}/environments/{env_name}/routes`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **Body** (`application/json`, required) `RouteRequest`
  - `host` (string/null, optional) - Omit or null to match any domain - which is the *widest* claim on this platform, not the mildest, and needs the widest grant.
  - `path_prefix` (string, required)
  - `priority` (integer, optional) - Higher wins when several routes match. Defaults to 0, and is bounded (`catalog::model::MAX_ROUTE_PRIORITY`) because it is a caller-supplied integer that becomes an ordering key at the edge.
- **201** `RouteResponse` - The route as created.
- **Refusals** `400`, `401`, `403`, `404`, `409` - see [Errors](#errors).

Example answer (`201`, `RouteResponse`):

```json
{
  "route": {
    "app_id": "demo",
    "env_id": "018f2518-7e38-7f80-8000-000000000001",
    "host": "demo.example.com",
    "id": "018f2518-7e38-7f80-8000-000000000001",
    "path_prefix": "/",
    "priority": 0
  }
}
```

#### `PUT /api/v1/apps/{app_id}/environments/{env_name}/routes/{route_id}`

Replaces the route's fields wholesale, so an omitted `host` clears it back to "any domain" rather than keeping the old value.

`operationId: updateRoute`. Legacy spelling `PUT /api/apps/{app_id}/environments/{env_name}/routes/{route_id}`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **Path** `route_id` (string, required) - The route's id.
- **Body** (`application/json`, required) `RouteRequest`
  - `host` (string/null, optional) - Omit or null to match any domain - which is the *widest* claim on this platform, not the mildest, and needs the widest grant.
  - `path_prefix` (string, required)
  - `priority` (integer, optional) - Higher wins when several routes match. Defaults to 0, and is bounded (`catalog::model::MAX_ROUTE_PRIORITY`) because it is a caller-supplied integer that becomes an ordering key at the edge.
- **200** `RouteResponse` - The route after the change.
- **Refusals** `400`, `401`, `403`, `404`, `409` - see [Errors](#errors).

#### `DELETE /api/v1/apps/{app_id}/environments/{env_name}/routes/{route_id}`

`operationId: deleteRoute`. Legacy spelling `DELETE /api/apps/{app_id}/environments/{env_name}/routes/{route_id}`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **Path** `route_id` (string, required) - The route's id.
- **200** `RouteDeleted` - The route was removed.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `RouteDeleted`):

```json
{
  "deleted": "018f2518-7e38-7f80-8000-000000000001"
}
```

### Secrets and storage tokens

No endpoint on this API returns a secret value: `read` reaches the names, the
revisions and the timestamps, and `secrets` writes and deletes values. A
write or a delete re-plans the running deployment so the nodes get a freshly
decrypted environment, which is why these answers carry a delivery report and
sometimes a `202` with a plan to poll. The storage token is the sharpest edge
on the API: what comes back talks to deis-store directly as one tenant,
without passing through this process again, and never expires.

#### `GET /api/v1/apps/{app_id}/environments/{env_name}/secrets`

Names and timestamps only. There is deliberately no endpoint that returns a secret value.

`operationId: listSecrets`. Legacy spelling `GET /api/apps/{app_id}/environments/{env_name}/secrets`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_SecretMeta` - One page of secret names and revisions; never values.
- **Refusals** `400`, `401`, `403`, `404`, `503` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_SecretMeta`):

```json
{
  "items": [
    {
      "name": "API_KEY",
      "revision": "018f2518-7e38-7f80-8000-000000000001",
      "updated_at": 1767225600
    }
  ],
  "page": {
    "count": 1,
    "limit": 100,
    "nextCursor": null,
    "total": 1
  },
  "schemaVersion": 1
}
```

#### `PUT /api/v1/apps/{app_id}/environments/{env_name}/secrets/{name}`

`operationId: putSecret`. Legacy spelling `PUT /api/apps/{app_id}/environments/{env_name}/secrets/{name}`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **Path** `name` (string, required) - The secret's name.
- **Body** (`application/json`, required) `PutSecretRequest`
  - `value` (string, required) - Plaintext value. It is encrypted before it touches the database and is never written to a log.
- **200** `SecretChangeResponse` - The secret was stored and delivered to every node.
- **202** `SecretChangeResponse` with `Location` - The secret was stored; the running deployment is being re-planned, poll `Location`.
- **Refusals** `400`, `401`, `403`, `404`, `412`, `503` - see [Errors](#errors).

Example answer (`200`, `SecretChangeResponse`):

```json
{
  "delivery": {
    "app_id": "demo",
    "delivered": [
      "runtime-test"
    ],
    "environment": "production",
    "failed": [],
    "phase": "legacy_apply",
    "reason": "secret_change",
    "skipped": null,
    "version_id": "018f2518-7e38-7f80-8000-000000000001"
  },
  "secret": {
    "name": "API_KEY",
    "revision": "018f2518-7e38-7f80-8000-000000000001",
    "updated_at": 1767225600
  }
}
```

#### `DELETE /api/v1/apps/{app_id}/environments/{env_name}/secrets/{name}`

`operationId: deleteSecret`. Legacy spelling `DELETE /api/apps/{app_id}/environments/{env_name}/secrets/{name}`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **Path** `name` (string, required) - The secret's name.
- **200** `SecretDeletedResponse` - The secret was removed and every node told.
- **202** `SecretDeletedResponse` with `Location` - The secret was removed; the running deployment is being re-planned, poll `Location`.
- **Refusals** `401`, `403`, `404`, `412`, `503` - see [Errors](#errors).

Example answer (`200`, `SecretDeletedResponse`):

```json
{
  "deleted": "API_KEY",
  "delivery": {
    "app_id": "demo",
    "delivered": [
      "runtime-test"
    ],
    "environment": "production",
    "failed": [],
    "phase": "legacy_apply",
    "reason": "secret_change",
    "skipped": null,
    "version_id": "018f2518-7e38-7f80-8000-000000000001"
  }
}
```

#### `GET /api/v1/apps/{app_id}/environments/{env_name}/storage-token`

Answers 404 when the application or the environment is unknown - the same resolution every other endpoint under this prefix does - and 409 when this control plane is not configured to reach or authorise storage at all.

`operationId: getStorageToken`. Legacy spelling `GET /api/apps/{app_id}/environments/{env_name}/storage-token`, deprecated.

- **Path** `app_id` (string, required) - The application's id.
- **Path** `env_name` (string, required) - The environment's name.
- **200** `StorageTokenResponse` - A credential for the environment's storage tenant.
- **Refusals** `401`, `403`, `404`, `409`, `503` - see [Errors](#errors).

### Hostname grants

The registry that decides which application may claim which hostname. It is a
statement about the whole platform - a grant decides which tenant receives
somebody else's cookies - so it is `admin:*/*` throughout and names an
application that is deliberately not the caller's.

#### `GET /api/v1/domains`

The whole registry in `(pattern, app_id, environment)` order. The scope is global: a hostname belongs to nobody's application, which is the whole point of the table.

`operationId: listDomainGrants`. Legacy spelling `GET /api/domains`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_DomainGrant` - One page of hostname grants.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_DomainGrant`):

```json
{
  "items": [
    {
      "app_id": "demo",
      "created_at": 1767225600,
      "environment": "*",
      "note": null,
      "pattern": "demo.example.com"
    }
  ],
  "page": {
    "count": 1,
    "limit": 100,
    "nextCursor": null,
    "total": 1
  },
  "schemaVersion": 1
}
```

#### `POST /api/v1/domains`

`operationId: createDomainGrant`. Legacy spelling `POST /api/domains`, deprecated.

- **Body** (`application/json`, required) `GrantRequest`
  - `app_id` (string, required)
  - `environment` (string/null, optional) - Which environment of that application may claim it. Omitted means every environment.
  - `note` (string/null, optional) - Free text for whoever reads this list later. Never interpreted.
  - `pattern` (string, required) - `example.com`, `*.example.com` (every name strictly under the zone, exactly as a wildcard route matches), `(no-host)` (the route that names no host, and nothing else) or `*` (every name, including that route).
- **201** `GrantResponse` - The grant as created.
- **Refusals** `400`, `401`, `403`, `404`, `409` - see [Errors](#errors).

Example answer (`201`, `GrantResponse`):

```json
{
  "grant": {
    "app_id": "demo",
    "created_at": 1767225600,
    "environment": "*",
    "note": null,
    "pattern": "demo.example.com"
  }
}
```

#### `DELETE /api/v1/domains/{pattern}`

The pattern travels in the path because that is what it is - the name of the thing - and the grantee in the query because a pattern can be held by more than one application and a delete has to say which one it means.

`operationId: deleteDomainGrant`. Legacy spelling `DELETE /api/domains/{pattern}`, deprecated.

- **Path** `pattern` (string, required) - The hostname pattern the grant holds.
- **Query** `app_id` (string, required) - The application that holds the grant.
- **Query** `environment` (string, optional) - The environment the grant was made for. Absent means the grant that covers every environment (`*`).
- **200** `GrantWithdrawn` - The grant was withdrawn.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `GrantWithdrawn`):

```json
{
  "app_id": "demo",
  "environment": "*",
  "withdrawn": "demo.example.com"
}
```

### Accounts, tokens and the audit log

Accounts, the tokens minted for them, and the log of everything anybody did.
A token's scopes can never exceed its account's role. A minted secret is
shown once, at creation, and is never readable again.

#### `GET /api/v1/accounts`

The unified list envelope, oldest account first. There is one scope (the whole installation) and no filter, so the cursor binding only has to name the sort.

`operationId: listAccounts`. Legacy spelling `GET /api/accounts`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_Account` - One page of accounts.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_Account`):

```json
{
  "items": [
    {
      "created_at": 1767225600,
      "disabled_at": null,
      "id": "018f2518-7e38-7f80-8000-000000000001",
      "name": "alice",
      "role": "viewer"
    }
  ],
  "page": {
    "count": 1,
    "limit": 100,
    "nextCursor": null,
    "total": 1
  },
  "schemaVersion": 1
}
```

#### `POST /api/v1/accounts`

`operationId: createAccount`. Legacy spelling `POST /api/accounts`, deprecated.

- **Body** (`application/json`, required) `CreateAccountRequest`
  - `name` (string, required)
  - `role` (string, required) - One of the names in [`Role`]. Required rather than defaulted: the difference between `viewer` and `owner` is the whole point, and a default would be the one somebody forgot to think about.
- **200** `Account` - The account as created.
- **Refusals** `400`, `401`, `403`, `409` - see [Errors](#errors).

Example answer (`200`, `Account`):

```json
{
  "created_at": 1767225600,
  "disabled_at": null,
  "id": "018f2518-7e38-7f80-8000-000000000001",
  "name": "alice",
  "role": "viewer"
}
```

#### `POST /api/v1/accounts/{account}/tokens`

`operationId: createToken`. Legacy spelling `POST /api/accounts/{account}/tokens`, deprecated.

- **Path** `account` (string, required) - The account the token belongs to.
- **Body** (`application/json`, required) `CreateTokenRequest`
  - `name` (string, required) - What this credential is for, in a word: `ci-preview`, `alice-laptop`. It is printed in every listing, so it is how a person decides which one to revoke.
  - `scopes` (array/null, optional) - The scope list. Absent means the account's role over every application (see [`Role::default_scopes`]); an explicit empty list means a token that can do nothing yet.
  - `unscoped` (boolean, optional) - Ask for a credential subject to no scope at all - the old flat `api_keys` behaviour. Spelled out rather than implied by an empty scope list, because "everything" and "nothing" must not be one typo apart.
- **200** `MintResponse` - The token and, once only, its secret.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `GET /api/v1/accounts/{name}`

`operationId: getAccount`. Legacy spelling `GET /api/accounts/{name}`, deprecated.

- **Path** `name` (string, required) - The account's name.
- **200** `Account` - The account.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

#### `PATCH /api/v1/accounts/{name}`

`operationId: updateAccount`. Legacy spelling `PATCH /api/accounts/{name}`, deprecated.

- **Path** `name` (string, required) - The account's name.
- **Body** (`application/json`, required) `UpdateAccountRequest`
  - `disabled` (boolean/null, optional) - Absent means "leave it". `true` refuses every token of this account without revoking any of them.
  - `role` (string/null, optional) - Absent means "leave it".
- **200** `Account` - The account after the change.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `GET /api/v1/audit`

`operationId: listAuditLog`. Legacy spelling `GET /api/audit`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **Query** `app` (string, optional) - The application's name as the audit line recorded it from the request path. A line outlives the application it names, so it is matched verbatim rather than resolved to an id.
- **Query** `environment` (string, optional) - The environment's name as the audit line recorded it, matched verbatim like `app`.
- **Query** `account` (string, optional) - The account's name, as it appears in the `actor_account` column.
- **Query** `action` (string, optional) - One of the action names the route table records - `deploy`, `secret.write`, `token.mint`.
- **Query** `outcome` (string, optional) - `accepted`, `refused` or `failed`.
- **Query** `since` (integer, optional) - Unix seconds, inclusive.
- **Query** `until` (integer, optional) - Unix seconds, inclusive; the upper bound of the window `since` opens.
- **200** `ControlListPage_AuditEntry` - One page of audit lines, newest first.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_AuditEntry`):

```json
{
  "items": [
    {
      "action": "token.revoke",
      "actor_account": null,
      "actor_credential": null,
      "actor_kind": "anonymous",
      "app_id": null,
      "at": 1767225600,
      "detail": null,
      "environment": null,
      "id": 3,
      "method": "DELETE",
      "outcome": "accepted",
      "path": "/api/v1/tokens/0000000000000000",
      "peer": "127.0.0.1:40000",
      "requestId": "018f2518-7e38-7f80-8000-000000000001",
      "status": 200,
      "target": "0000000000000000"
    },
    {
      "action": "token.mint",
      "actor_account": null,
      "actor_credential": null,
      "actor_kind": "anonymous",
      "app_id": null,
      "at": 1767225600,
      "detail": null,
      "environment": null,
      "id": 2,
      "method": "POST",
      "outcome": "accepted",
      "path": "/api/v1/accounts/alice/tokens",
      "peer": "127.0.0.1:40000",
      "requestId": "018f2518-7e38-7f80-8000-000000000001",
      "status": 200,
      "target": "alice"
    },
    {
      "action": "account.create",
      "actor_account": null,
      "actor_credential": null,
      "actor_kind": "anonymous",
      "app_id": null,
      "at": 1767225600,
      "detail": null,
      "environment": null,
      "id": 1,
      "method": "POST",
      "outcome": "accepted",
      "path": "/api/v1/accounts",
      "peer": "127.0.0.1:40000",
      "requestId": "018f2518-7e38-7f80-8000-000000000001",
      "status": 200,
      "target": null
    }
  ],
  "page": {
    "count": 3,
    "limit": 100,
    "nextCursor": null,
    "total": 3
  },
  "schemaVersion": 1
}
```

#### `GET /api/v1/tokens`

The unified list envelope, oldest credential first. Every filter is part of the cursor binding - by the account's id, never its name - so a cursor issued for one listing cannot be resumed under another.

`operationId: listTokens`. Legacy spelling `GET /api/tokens`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **Query** `account` (string, optional) - An account's name. Resolved to its id before anything is read, so an unknown name is a 404 rather than an empty list.
- **Query** `include_revoked` (boolean, optional) - Include credentials that no longer work. Off by default so a listing answers "what can reach this platform right now".
- **Query** `scope` (string, optional) - `all` (the default) or `unscoped`. Under `unscoped` the page total is the number of credentials that answer to no scope, which is what `deis token list` warns about; a caller that only wants the number asks for `scope=unscoped&limit=1` and reads `page.total`.
- **200** `ControlListPage_ApiToken` - One page of tokens.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_ApiToken`):

```json
{
  "items": [
    {
      "account": "alice",
      "account_id": "018f2518-7e38-7f80-8000-000000000001",
      "created_at": 1767225600,
      "id": "0000000000000000",
      "last_used_at": null,
      "name": "laptop",
      "origin": "minted",
      "revoked_at": null,
      "scopes": "read:*/*",
      "unscoped": false
    }
  ],
  "page": {
    "count": 1,
    "limit": 100,
    "nextCursor": null,
    "total": 1
  },
  "schemaVersion": 1
}
```

#### `DELETE /api/v1/tokens/{id}`

`operationId: revokeToken`. Legacy spelling `DELETE /api/tokens/{id}`, deprecated.

- **Path** `id` (string, required) - The token's id.
- **200** `ApiToken` - The token, now revoked.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `ApiToken`):

```json
{
  "account": "alice",
  "account_id": "018f2518-7e38-7f80-8000-000000000001",
  "created_at": 1767225600,
  "id": "0000000000000000",
  "last_used_at": null,
  "name": "laptop",
  "origin": "minted",
  "revoked_at": 1767225600,
  "scopes": "read:*/*",
  "unscoped": false
}
```

### Providers

The provider-neutral management surface, in the order the records depend on
each other: a **driver release** is verified and installed, an
**installation** binds an account of that provider, a **target** is
registered against an installation, and an administrator **grants** a target
to one application environment. Only then can a deployment credential start a
**provider deployment**, and only through that durable, revision-pinned
grant.

Credential values enter through the provisioning handlers and are never
returned, logged or copied into an operation. Every create requires an
`Idempotency-Key`; every later change to an existing record - disable,
enable, delete, revoke, rotate - requires both that and an `If-Match`. A
repeated key with the same request replays its answer; with a different
request it is refused with `idempotency-key-reused`. Installations and
targets are soft-deleted: the record comes back carrying a deletion
timestamp, and one that still backs an active grant or deployment is refused
with one of the `provider-*` conflict codes instead.

#### `GET /api/v1/apps/{app_name}/environments/{environment_name}/provider-deployments`

`operationId: listProviderDeployments`. Legacy spelling `GET /api/apps/{app_name}/environments/{environment_name}/provider-deployments`, deprecated.

- **Path** `app_name` (string, required) - The application's id.
- **Path** `environment_name` (string, required) - The environment's name.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderDeploymentWorkflow` - One page of provider deployments, newest first.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `POST /api/v1/apps/{app_name}/environments/{environment_name}/provider-deployments`

`operationId: beginProviderDeployment`. Legacy spelling `POST /api/apps/{app_name}/environments/{environment_name}/provider-deployments`, deprecated.

- **Path** `app_name` (string, required) - The application's id.
- **Path** `environment_name` (string, required) - The environment's name.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Body** (`application/json`, required) `BeginProviderDeploymentRequest`
  - `artifact` (`ArtifactVersionRef`, required)
  - `plan` (`ProviderNeutralPlan`, optional)
  - `providerTargetId` (string, required)
- **202** `ProductOperation` with `Location` - The deployment was recorded and will be driven; poll `Location`.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `428`, `503` - see [Errors](#errors).

#### `GET /api/v1/apps/{app_name}/environments/{environment_name}/provider-deployments/{operation_id}`

`operationId: getProviderDeployment`. Legacy spelling `GET /api/apps/{app_name}/environments/{environment_name}/provider-deployments/{operation_id}`, deprecated.

- **Path** `app_name` (string, required) - The application's id.
- **Path** `environment_name` (string, required) - The environment's name.
- **Path** `operation_id` (string, required) - The deployment operation's UUIDv7.
- **200** `ProviderDeploymentWorkflow` with `ETag` - The workflow and its operation.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `POST /api/v1/apps/{app_name}/environments/{environment_name}/provider-deployments/{operation_id}/cancel`

`operationId: cancelProviderDeployment`. Legacy spelling `POST /api/apps/{app_name}/environments/{environment_name}/provider-deployments/{operation_id}/cancel`, deprecated.

- **Path** `app_name` (string, required) - The application's id.
- **Path** `environment_name` (string, required) - The environment's name.
- **Path** `operation_id` (string, required) - The deployment or rollback operation's UUIDv7.
- **Header** `Idempotency-Key` (string, optional) - A replay of the same key by the same principal returns the same operation.
- **Header** `If-Match` (string, required) - The operation revision the caller expects.
- **202** `ProductOperation` with `Location` - Cancellation was requested; poll `Location`. The operation is `cancelling` until in-flight provider work settles and reversible side effects are cleaned up, then `cancelled` (or `failed` with the cleanup diagnostic). Steps that already activated are not undone; that is a rollback.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `428`, `503` - see [Errors](#errors). A terminal or already-cancelling operation answers `409` `operation-not-cancellable`.

#### `POST /api/v1/apps/{app_name}/environments/{environment_name}/provider-deployments/{operation_id}/rollback`

`operationId: rollbackProviderDeployment`. Legacy spelling `POST /api/apps/{app_name}/environments/{environment_name}/provider-deployments/{operation_id}/rollback`, deprecated.

- **Path** `app_name` (string, required) - The application's id.
- **Path** `environment_name` (string, required) - The environment's name.
- **Path** `operation_id` (string, required) - The succeeded deployment operation to roll back to.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same operation.
- **202** `ProductOperation` with `Location` - A new `rollback` operation was recorded and will be driven; poll `Location`. Its workflow names the deployment in `rolledBackFromOperationId`, runs the driver's `rollback` against that deployment's exact desired state and then `observe`s the result.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `428`, `503` - see [Errors](#errors). A source that is not a succeeded deployment on this environment, or whose target grant is no longer the active one, answers `409` `operation-not-rollbackable`.

#### `GET /api/v1/apps/{app_name}/environments/{environment_name}/provider-target-grants`

`operationId: listProviderTargetGrants`. Legacy spelling `GET /api/apps/{app_name}/environments/{environment_name}/provider-target-grants`, deprecated.

- **Path** `app_name` (string, required) - The application's id.
- **Path** `environment_name` (string, required) - The environment's name.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **Query** `status` (string, optional) - `active` (the default), `revoked` or `all`.
- **200** `ControlListPage_ProviderTargetGrant` - One page of the environment's grants.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `POST /api/v1/apps/{app_name}/environments/{environment_name}/provider-target-grants`

`operationId: grantProviderTarget`. Legacy spelling `POST /api/apps/{app_name}/environments/{environment_name}/provider-target-grants`, deprecated.

- **Path** `app_name` (string, required) - The application's id.
- **Path** `environment_name` (string, required) - The environment's name.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Body** (`application/json`, required) `GrantProviderTargetRequest`
  - `note` (string/null, optional)
  - `providerTargetId` (string, required)
- **201** `ProviderTargetGrant` with `ETag`, `Location` - The grant was recorded.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `428` - see [Errors](#errors).

#### `GET /api/v1/apps/{app_name}/environments/{environment_name}/provider-target-grants/{grant_id}`

`operationId: getProviderTargetGrant`. Legacy spelling `GET /api/apps/{app_name}/environments/{environment_name}/provider-target-grants/{grant_id}`, deprecated.

- **Path** `app_name` (string, required) - The application's id.
- **Path** `environment_name` (string, required) - The environment's name.
- **Path** `grant_id` (string, required) - The grant's UUIDv7.
- **200** `ProviderTargetGrant` with `ETag` - The grant.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `DELETE /api/v1/apps/{app_name}/environments/{environment_name}/provider-target-grants/{grant_id}`

`operationId: revokeProviderTargetGrant`. Legacy spelling `DELETE /api/apps/{app_name}/environments/{environment_name}/provider-target-grants/{grant_id}`, deprecated.

- **Path** `app_name` (string, required) - The application's id.
- **Path** `environment_name` (string, required) - The environment's name.
- **Path** `grant_id` (string, required) - The grant's UUIDv7.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Header** `If-Match` (string, required) - The revision the caller expects, from `ETag`.
- **200** `ProviderTargetGrant` with `ETag` - The grant, now revoked.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `428` - see [Errors](#errors).

#### `GET /api/v1/apps/{app_name}/environments/{environment_name}/provider-target-grants/{grant_id}/audit`

`operationId: listProviderTargetGrantAudit`. Legacy spelling `GET /api/apps/{app_name}/environments/{environment_name}/provider-target-grants/{grant_id}/audit`, deprecated.

- **Path** `app_name` (string, required) - The application's id.
- **Path** `environment_name` (string, required) - The environment's name.
- **Path** `grant_id` (string, required) - The grant's UUIDv7.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderTargetGrantAuditEvent` - One page of grant events, newest first.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `GET /api/v1/providers/credentials`

`operationId: listProviderCredentials`. Legacy spelling `GET /api/providers/credentials`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderCredentialBinding` - One page of bindings; never a credential value.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_ProviderCredentialBinding`):

```json
{
  "items": [],
  "page": {
    "count": 0,
    "limit": 100,
    "nextCursor": null,
    "total": 0
  },
  "schemaVersion": 1
}
```

#### `POST /api/v1/providers/credentials`

`operationId: provisionProviderCredential`. Legacy spelling `POST /api/providers/credentials`, deprecated.

- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Body** (`application/json`, required) `ProvisionCredentialRequest`
  - `backendId` (string, required)
  - `credential` (string, required)
  - `name` (string, required)
- **201** `ProviderCredentialBinding` with `ETag`, `Location` - The credential was sealed and bound.
- **Refusals** `400`, `401`, `403`, `409`, `413`, `428`, `503` - see [Errors](#errors).

#### `GET /api/v1/providers/credentials/{binding_id}`

`operationId: getProviderCredential`. Legacy spelling `GET /api/providers/credentials/{binding_id}`, deprecated.

- **Path** `binding_id` (string, required) - The credential binding's UUIDv7.
- **200** `ProviderCredentialBinding` with `ETag` - The binding.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `DELETE /api/v1/providers/credentials/{binding_id}`

`operationId: revokeProviderCredential`. Legacy spelling `DELETE /api/providers/credentials/{binding_id}`, deprecated.

- **Path** `binding_id` (string, required) - The credential binding's UUIDv7.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Header** `If-Match` (string, required) - The revision the caller expects, from `ETag`.
- **200** `ProviderCredentialBinding` with `ETag` - The binding, now revoked.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `428` - see [Errors](#errors).

#### `GET /api/v1/providers/credentials/{binding_id}/audit`

`operationId: listProviderCredentialAudit`. Legacy spelling `GET /api/providers/credentials/{binding_id}/audit`, deprecated.

- **Path** `binding_id` (string, required) - The credential binding's UUIDv7.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderCredentialAuditEvent` - One page of audit events, newest first.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `POST /api/v1/providers/credentials/{binding_id}/rotate`

`operationId: rotateProviderCredential`. Legacy spelling `POST /api/providers/credentials/{binding_id}/rotate`, deprecated.

- **Path** `binding_id` (string, required) - The credential binding's UUIDv7.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Header** `If-Match` (string, required) - The revision the caller expects, from `ETag`.
- **Body** (`application/json`, required) `RotateCredentialRequest`
  - `credential` (string, required)
- **200** `ProviderCredentialBinding` with `ETag` - The binding at its new revision.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `413`, `428`, `503` - see [Errors](#errors).

#### `GET /api/v1/providers/driver-releases`

`operationId: listDriverReleases`. Legacy spelling `GET /api/providers/driver-releases`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderDriverReleaseRecord` - One page of installed releases.
- **Refusals** `400`, `401`, `403`, `503` - see [Errors](#errors).

#### `POST /api/v1/providers/driver-releases`

`operationId: installDriverRelease`. Legacy spelling `POST /api/providers/driver-releases`, deprecated.

- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Body** (`multipart/form-data`, required) `DriverReleaseUpload`
  - `archive` (string, required) - The driver archive the release names.
  - `authorization` (string, required) - The release authorization envelope, canonical JSON (`contracts/v1/provider-driver-release-envelope.schema.json`).
  - `manifest` (string, required) - The driver manifest, canonical JSON (`contracts/v1/provider-driver.schema.json`).
  - `release` (string, required) - The release document, canonical JSON (`contracts/v1/provider-driver-release.schema.json`).
  - `transparency` (string, required) - The transparency proof, canonical JSON (`contracts/v1/provider-driver-transparency-proof.schema.json`).
- **200** `ProviderDriverReleaseRecord` with `ETag` - The same release was already installed.
- **201** `ProviderDriverReleaseRecord` with `ETag`, `Location` - The release was verified and installed.
- **Refusals** `400`, `401`, `403`, `409`, `413`, `422`, `428`, `503` - see [Errors](#errors).

#### `GET /api/v1/providers/driver-releases/{release_digest}`

`operationId: getDriverRelease`. Legacy spelling `GET /api/providers/driver-releases/{release_digest}`, deprecated.

- **Path** `release_digest` (string, required) - The release digest, 64 lowercase hexadecimal digits.
- **200** `ProviderDriverReleaseRecord` with `ETag` - The release.
- **Refusals** `400`, `401`, `403`, `404`, `503` - see [Errors](#errors).

#### `GET /api/v1/providers/driver-releases/{release_digest}/audit`

`operationId: listDriverReleaseAudit`. Legacy spelling `GET /api/providers/driver-releases/{release_digest}/audit`, deprecated.

- **Path** `release_digest` (string, required) - The release digest, 64 lowercase hexadecimal digits.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderDriverReleaseAuditEvent` - One page of audit events, newest first.
- **Refusals** `400`, `401`, `403`, `404`, `503` - see [Errors](#errors).

#### `GET /api/v1/providers/installations`

`operationId: listProviderInstallations`. Legacy spelling `GET /api/providers/installations`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderInstallation` - One page of installations.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_ProviderInstallation`):

```json
{
  "items": [],
  "page": {
    "count": 0,
    "limit": 100,
    "nextCursor": null,
    "total": 0
  },
  "schemaVersion": 1
}
```

#### `POST /api/v1/providers/installations`

`operationId: registerProviderInstallation`. Legacy spelling `POST /api/providers/installations`, deprecated.

- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Body** (`application/json`, required) `RegisterInstallationRequest`
  - `accountReference` (string, required)
  - `backendId` (string, required)
  - `credentialBindingId` (string, required)
  - `region` (string/null, optional)
- **201** `ProviderInstallation` with `ETag`, `Location` - The installation was registered.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `428` - see [Errors](#errors).

#### `GET /api/v1/providers/installations/{installation_id}`

`operationId: getProviderInstallation`. Legacy spelling `GET /api/providers/installations/{installation_id}`, deprecated.

- **Path** `installation_id` (string, required) - The installation's UUIDv7.
- **200** `ProviderInstallation` with `ETag` - The installation.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `DELETE /api/v1/providers/installations/{installation_id}`

`operationId: deleteProviderInstallation`. Legacy spelling `DELETE /api/providers/installations/{installation_id}`, deprecated.

- **Path** `installation_id` (string, required) - The installation's UUIDv7.
- **Query** `cascade` (boolean, optional) - Also revoke the installation's targets and active grants. Off by default, in which case an installation with dependents is refused with `provider-installation-has-dependents`.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Header** `If-Match` (string, required) - The revision the caller expects, from `ETag`.
- **200** `ProviderInstallation` with `ETag` - The installation, now deleted; with `cascade=true` its targets and their grants too.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `428` - see [Errors](#errors).

#### `GET /api/v1/providers/installations/{installation_id}/audit`

`operationId: listProviderInstallationAudit`. Legacy spelling `GET /api/providers/installations/{installation_id}/audit`, deprecated.

- **Path** `installation_id` (string, required) - The installation's UUIDv7.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderLifecycleAuditEvent` - One page of lifecycle events, newest first.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `POST /api/v1/providers/installations/{installation_id}/disable`

`operationId: disableProviderInstallation`. Legacy spelling `POST /api/providers/installations/{installation_id}/disable`, deprecated.

- **Path** `installation_id` (string, required) - The installation's UUIDv7.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Header** `If-Match` (string, required) - The revision the caller expects, from `ETag`.
- **200** `ProviderInstallation` with `ETag` - The installation, now disabled.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `428` - see [Errors](#errors).

#### `POST /api/v1/providers/installations/{installation_id}/enable`

`operationId: enableProviderInstallation`. Legacy spelling `POST /api/providers/installations/{installation_id}/enable`, deprecated.

- **Path** `installation_id` (string, required) - The installation's UUIDv7.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Header** `If-Match` (string, required) - The revision the caller expects, from `ETag`.
- **200** `ProviderInstallation` with `ETag` - The installation, now enabled.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `428` - see [Errors](#errors).

#### `GET /api/v1/providers/installations/{installation_id}/targets`

`operationId: listProviderInstallationTargets`. Legacy spelling `GET /api/providers/installations/{installation_id}/targets`, deprecated.

- **Path** `installation_id` (string, required) - The installation's UUIDv7.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderTarget` - One page of the installation's targets.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `POST /api/v1/providers/installations/{installation_id}/targets`

`operationId: registerProviderTarget`. Legacy spelling `POST /api/providers/installations/{installation_id}/targets`, deprecated.

- **Path** `installation_id` (string, required) - The installation's UUIDv7.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Body** (`application/json`, required) `RegisterTargetRequest`
  - `driverManifest` (`ProviderDriverManifest`, required)
  - `name` (string, required)
  - `standardVersion` (string, required)
  - `targetConfigDigest` (string, required)
- **201** `ProviderTarget` with `ETag`, `Location` - The target was registered against an installed driver release.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `422`, `428`, `503` - see [Errors](#errors).

#### `GET /api/v1/providers/target-grants`

`operationId: listProviderTargetGrantHistory`. Legacy spelling `GET /api/providers/target-grants`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **Query** `status` (string, optional) - `all` (the default), `active` or `revoked`.
- **Query** `target` (string, optional) - A provider target's id; narrows the history to grants on that target.
- **200** `ControlListPage_ProviderTargetGrant` - One page of grants across every application.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_ProviderTargetGrant`):

```json
{
  "items": [],
  "page": {
    "count": 0,
    "limit": 100,
    "nextCursor": null,
    "total": 0
  },
  "schemaVersion": 1
}
```

#### `GET /api/v1/providers/target-grants/{grant_id}`

`operationId: getProviderTargetGrantHistory`. Legacy spelling `GET /api/providers/target-grants/{grant_id}`, deprecated.

- **Path** `grant_id` (string, required) - The grant's UUIDv7.
- **200** `ProviderTargetGrant` with `ETag` - The grant.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `GET /api/v1/providers/target-grants/{grant_id}/audit`

`operationId: listProviderTargetGrantHistoryAudit`. Legacy spelling `GET /api/providers/target-grants/{grant_id}/audit`, deprecated.

- **Path** `grant_id` (string, required) - The grant's UUIDv7.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderTargetGrantAuditEvent` - One page of grant events, newest first.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `GET /api/v1/providers/targets`

`operationId: listProviderTargets`. Legacy spelling `GET /api/providers/targets`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderTarget` - One page of targets.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_ProviderTarget`):

```json
{
  "items": [],
  "page": {
    "count": 0,
    "limit": 100,
    "nextCursor": null,
    "total": 0
  },
  "schemaVersion": 1
}
```

#### `GET /api/v1/providers/targets/{target_id}`

`operationId: getProviderTarget`. Legacy spelling `GET /api/providers/targets/{target_id}`, deprecated.

- **Path** `target_id` (string, required) - The target's UUIDv7.
- **200** `ProviderTarget` with `ETag` - The target.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `DELETE /api/v1/providers/targets/{target_id}`

`operationId: deleteProviderTarget`. Legacy spelling `DELETE /api/providers/targets/{target_id}`, deprecated.

- **Path** `target_id` (string, required) - The target's UUIDv7.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Header** `If-Match` (string, required) - The revision the caller expects, from `ETag`.
- **200** `ProviderTarget` with `ETag` - The target, now deleted, and its grants revoked.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `428` - see [Errors](#errors).

#### `GET /api/v1/providers/targets/{target_id}/audit`

`operationId: listProviderTargetAudit`. Legacy spelling `GET /api/providers/targets/{target_id}/audit`, deprecated.

- **Path** `target_id` (string, required) - The target's UUIDv7.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ProviderLifecycleAuditEvent` - One page of lifecycle events, newest first.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `POST /api/v1/providers/targets/{target_id}/disable`

`operationId: disableProviderTarget`. Legacy spelling `POST /api/providers/targets/{target_id}/disable`, deprecated.

- **Path** `target_id` (string, required) - The target's UUIDv7.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Header** `If-Match` (string, required) - The revision the caller expects, from `ETag`.
- **200** `ProviderTarget` with `ETag` - The target, now disabled.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `428` - see [Errors](#errors).

#### `POST /api/v1/providers/targets/{target_id}/enable`

`operationId: enableProviderTarget`. Legacy spelling `POST /api/providers/targets/{target_id}/enable`, deprecated.

- **Path** `target_id` (string, required) - The target's UUIDv7.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same answer.
- **Header** `If-Match` (string, required) - The revision the caller expects, from `ETag`.
- **200** `ProviderTarget` with `ETag` - The target, now enabled.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `428` - see [Errors](#errors).

### Long operations, identity migration and backend restores

The long-running work of the platform and the record that tracks it. A
`ProductOperation` is the same shape whichever mutation produced it; see
[Long operations](#long-operations) for how to poll and cancel one.

#### `GET /api/v1/apps/{application_name}/identity-migration`

`operationId: getIdentityMigration`. Legacy spelling `GET /api/apps/{application_name}/identity-migration`, deprecated.

- **Path** `application_name` (string, required) - The application's id.
- **200** `MigrationStatusResponse` - The migration's status.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

#### `POST /api/v1/apps/{application_name}/identity-migration`

`operationId: startIdentityMigration`. Legacy spelling `POST /api/apps/{application_name}/identity-migration`, deprecated.

- **Path** `application_name` (string, required) - The application's id.
- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same operation.
- **Header** `If-Match` (string, required) - The application revision the caller expects.
- **202** `ProductOperation` with `Location` - The migration was started or is already running; poll `Location`.
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `428`, `503` - see [Errors](#errors).

#### `POST /api/v1/backend-restores`

`operationId: startBackendRestore`. Legacy spelling `POST /api/backend-restores`, deprecated.

- **Header** `Idempotency-Key` (string, required) - Replays of the same key return the same operation.
- **Body** (`application/json`, required) `BackendRestoreSet`
  - `$schema` (string, required) - The `$id` of the schema the set conforms to.
  - `components` (array of object, required)
  - `consistencyBarrier` (object, required)
  - `createdAt` (string, required) - RFC 3339.
  - `restoreSetId` (string, required) - Canonical UUIDv7 of the set.
  - `schemaVersion` (integer, required) - Always 1.
  - `scope` (object, required)
  - `topologyDriver` (object, required)
- **202** `ProductOperation` with `Location` - The restore was planned; poll `Location`.
- **Refusals** `400`, `401`, `403`, `409`, `422`, `428`, `503` - see [Errors](#errors).

#### `GET /api/v1/backend-restores/{restore_id}`

`operationId: getBackendRestore`. Legacy spelling `GET /api/backend-restores/{restore_id}`, deprecated.

- **Path** `restore_id` (string, required) - The restore's canonical UUIDv7.
- **200** `BackendRestoreStatus` with `ETag` - The plan and its operation.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `GET /api/v1/installation`

`operationId: getProductInstallation`. Legacy spelling `GET /api/installation`, deprecated.

- **200** `ProductInstallation` - This control plane's installation record.
- **Refusals** `401`, `403`, `500` - see [Errors](#errors).

Example answer (`200`, `ProductInstallation`):

```json
{
  "createdAtUnixSeconds": 1767225600,
  "installationId": "018f2518-7e38-7f80-8000-000000000001",
  "organizationId": "018f2518-7e38-7f80-8000-000000000001",
  "revision": 1,
  "updatedAtUnixSeconds": 1767225600
}
```

#### `GET /api/v1/operations/{operation_id}`

`operationId: getOperation`. Legacy spelling `GET /api/operations/{operation_id}`, deprecated.

What the route demands of the caller is read from the operation's record
rather than from the path, exactly as the cancel below reads it:

| `kind` | Scope |
|---|---|
| `deploy`, `rollback` (a provider workflow) | `read:<app>/<env>` of the application and environment in the record - what `getProviderDeployment` demands for the same workflow |
| `migrate`, `restore`, anything else | `admin:*/*` |

A caller whose scope does not cover the operation is refused with `403`
`insufficient-scope` whether or not the operation exists, and with the same
body either way - the same fixed body the cancel gives - so a credential over
one environment learns nothing about ids it may not see. Only an
administrator is told `404` for an id that does not exist. The refusal is in
the audit log under `operation.get`; a successful read is not.

- **Path** `operation_id` (string, required) - The operation's canonical UUIDv7.
- **200** `ProductOperation` with `ETag` - The operation.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

#### `POST /api/v1/operations/{operation_id}/cancel`

`operationId: cancelOperation`. Legacy spelling `POST /api/operations/{operation_id}/cancel`, deprecated.

The route dispatches on the operation's `kind`, and what it demands of the
caller is read from the operation's record rather than from the path:

| `kind` | Scope | What happens |
|---|---|---|
| `deploy`, `rollback` (a provider workflow) | `deploy:<app>/<env>` of the application and environment in the record | Exactly what `cancelProviderDeployment` does: `cancelling` until in-flight provider work settles and reversible side effects are cleaned up, then `cancelled` (or `failed` with the cleanup diagnostic). `Location` is the scoped provider deployment, which the same credential can read. Terminal or already cancelling: `409` `operation-not-cancellable`. |
| `migrate`, `restore` | `admin:*/*` | The operation's own controller stops it; `Location` is this operation. |
| anything else | `admin:*/*` | `409`: this route has no cancellation controller for it. |

A caller whose scope does not cover the operation is refused with `403`
`insufficient-scope` whether or not the operation exists, and with the same
body either way: the refusal names the scope template of each kind
(`deploy:<app>/<env>`, `admin:*/*`), never the application, environment or
kind in the record, so a credential over one environment learns nothing about
ids it may not see. The exact demand is written to the server log with the
request id.

- **Path** `operation_id` (string, required) - The operation's canonical UUIDv7.
- **Header** `Idempotency-Key` (string, optional) - For a provider `deploy` or `rollback` a replay of the same key by the same principal returns the same operation, exactly as on `cancelProviderDeployment`; without one a redelivered cancel finds nothing left to cancel and answers `409`. An identity migration or backend restore cancel is fenced by `If-Match` alone.
- **Header** `If-Match` (string, required) - The operation revision the caller expects.
- **202** `ProductOperation` with `Location` - Cancellation was requested; poll `Location` (the scoped provider deployment for a provider `deploy` or `rollback`, this operation otherwise).
- **Refusals** `400`, `401`, `403`, `404`, `409`, `412`, `428`, `503` - see [Errors](#errors). A terminal or already-cancelling provider operation answers `409` `operation-not-cancellable`.

---

## Platform diagnostics - `/api/platform`

Operator diagnostics: the machines, not the applications. Every route here is
`platform`-tagged in the OpenAPI document and demands `read:*/*` to read and
`admin:*/*` to write, with one exception - the bare fleet list, which any
scope answers because `deis status` prints it.

Two lists in here are easy to confuse and are not the same thing:

- **Control channels** (`/api/platform/sessions`) are the nodes currently
  holding a gRPC control channel open to this process: they authenticated,
  registered and are heartbeating. A command or a configuration push travels
  down one of these, so a node that is not on this list cannot be sent
  either. In practice deis-proxy is what appears here.
- **The runtime fleet** (`/api/platform/runtime`) is the machines that
  receive deployments and run cron occurrences. It is built from the
  configuration file plus self-registrations rather than from open channels.

The two overlap in nothing but the word "node".

### Control channels

The nodes currently holding a gRPC control channel open, and their heartbeat
history.

#### `GET /api/platform/sessions`

`operationId: listSessions`. Legacy spelling `GET /api/sessions`, deprecated.

- **200** `SessionsResponse` - Every open control channel.
- **Refusals** `401`, `403` - see [Errors](#errors).

Example answer (`200`, `SessionsResponse`):

```json
{
  "sessions": [],
  "total": 0
}
```

#### `GET /api/platform/sessions/{node_id}`

`operationId: getSession`. Legacy spelling `GET /api/sessions/{node_id}`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **200** `SessionView` - The open control channel.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

#### `GET /api/platform/sessions/{node_id}/heartbeats`

`operationId: listSessionHeartbeats`. Legacy spelling `GET /api/sessions/{node_id}/heartbeats`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_HeartbeatView` - One page of heartbeats, newest first.
- **Refusals** `400`, `401`, `403`, `500` - see [Errors](#errors).

### Node configuration

Key-value configuration held for each node, its change history, and the push
that sends it down that node's gRPC control channel. A node that holds no
open channel cannot be pushed to.

#### `GET /api/platform/configs`

`operationId: listNodeConfigs`. Legacy spelling `GET /api/configs`, deprecated.

- **200** `ConfigsListing` - Every node's configuration.
- **Refusals** `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ConfigsListing`):

```json
{
  "count": 1,
  "data": {
    "node-a": {
      "port": 8080
    }
  },
  "success": true
}
```

#### `POST /api/platform/configs/push/{node_id}`

`operationId: pushNodeConfigs`. Legacy spelling `POST /api/configs/push/{node_id}`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **200** `ConfigsPushed` - The node acknowledged every key.
- **Refusals** `401`, `403`, `404`, `503` - see [Errors](#errors).

#### `POST /api/platform/configs/push/{node_id}/{config_key}`

`operationId: pushNodeConfigValue`. Legacy spelling `POST /api/configs/push/{node_id}/{config_key}`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **Path** `config_key` (string, required) - The configuration key.
- **200** `ConfigKeyOutcome` - The node acknowledged the key.
- **Refusals** `401`, `403`, `404`, `503` - see [Errors](#errors).

#### `GET /api/platform/configs/stats`

`operationId: getNodeConfigStats`. Legacy spelling `GET /api/configs/stats`, deprecated.

- **200** `ConfigStatsResponse` - Counts of nodes and keys.
- **Refusals** `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ConfigStatsResponse`):

```json
{
  "node_count": 1,
  "success": true,
  "total_configs": 1
}
```

#### `GET /api/platform/configs/{node_id}`

`operationId: getNodeConfigs`. Legacy spelling `GET /api/configs/{node_id}`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **200** `NodeConfigs` - The node's configuration.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `NodeConfigs`):

```json
{
  "count": 1,
  "data": {
    "port": 8080
  },
  "node_id": "node-a",
  "success": true
}
```

#### `POST /api/platform/configs/{node_id}`

`operationId: updateNodeConfigs`. Legacy spelling `POST /api/configs/{node_id}`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **Body** (`application/json`, required) `UpdateConfigRequest`
  - `configs` (object, required)
- **200** `ConfigsUpdated` - The keys were recorded.
- **Refusals** `400`, `401`, `403`, `500` - see [Errors](#errors).

Example answer (`200`, `ConfigsUpdated`):

```json
{
  "message": "Configs updated successfully",
  "node_id": "node-a",
  "success": true,
  "updated_keys": [
    "port"
  ],
  "version_timestamp": "2026-01-01T00:00:00Z"
}
```

#### `DELETE /api/platform/configs/{node_id}`

`operationId: deleteNodeConfigs`. Legacy spelling `DELETE /api/configs/{node_id}`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **200** `NodeConfigOutcome` - The node's configuration was removed.
- **Refusals** `401`, `403`, `500` - see [Errors](#errors).

Example answer (`200`, `NodeConfigOutcome`):

```json
{
  "message": "All configs deleted successfully",
  "node_id": "node-a",
  "success": true
}
```

#### `GET /api/platform/configs/{node_id}/history`

`operationId: listNodeConfigHistory`. Legacy spelling `GET /api/configs/{node_id}/history`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ConfigHistoryItem` - One page of changes, newest first.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_ConfigHistoryItem`):

```json
{
  "items": [
    {
      "config_key": "port",
      "config_value": 8080,
      "created_at": "2026-01-01T00:00:00Z",
      "id": 1,
      "operation": "create",
      "version_timestamp": "2026-01-01T00:00:00Z"
    }
  ],
  "page": {
    "count": 1,
    "limit": 100,
    "nextCursor": null,
    "total": 1
  },
  "schemaVersion": 1
}
```

#### `GET /api/platform/configs/{node_id}/{config_key}`

`operationId: getNodeConfigValue`. Legacy spelling `GET /api/configs/{node_id}/{config_key}`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **Path** `config_key` (string, required) - The configuration key.
- **200** `ConfigValue` - The value.
- **Refusals** `401`, `403`, `404` - see [Errors](#errors).

Example answer (`200`, `ConfigValue`):

```json
{
  "config_key": "port",
  "node_id": "node-a",
  "success": true,
  "value": 8080
}
```

#### `DELETE /api/platform/configs/{node_id}/{config_key}`

`operationId: deleteNodeConfigValue`. Legacy spelling `DELETE /api/configs/{node_id}/{config_key}`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **Path** `config_key` (string, required) - The configuration key.
- **200** `ConfigKeyOutcome` - The key was removed.
- **Refusals** `401`, `403`, `500` - see [Errors](#errors).

Example answer (`200`, `ConfigKeyOutcome`):

```json
{
  "config_key": "port",
  "message": "Config deleted successfully",
  "node_id": "node-a",
  "success": true
}
```

#### `GET /api/platform/configs/{node_id}/{config_key}/history`

`operationId: listNodeConfigKeyHistory`. Legacy spelling `GET /api/configs/{node_id}/{config_key}/history`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **Path** `config_key` (string, required) - The configuration key.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_ConfigHistoryItem` - One page of changes to the key, newest first.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

#### `POST /api/platform/configs/{node_id}/{config_key}/rollback`

`operationId: rollbackNodeConfigValue`. Legacy spelling `POST /api/configs/{node_id}/{config_key}/rollback`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **Path** `config_key` (string, required) - The configuration key.
- **Body** (`application/json`, required) `RollbackRequest`
  - `version_timestamp` (string, required) - The `version_timestamp` of a history entry for this key.
- **200** `ConfigRolledBack` - The value was restored.
- **Refusals** `400`, `401`, `403`, `404`, `500` - see [Errors](#errors).

### Node commands

Commands travel down a node's gRPC control channel, so a node that is not on
the control-channel list cannot be sent one. Submitting a command answers
immediately with its id; the result is read back from its status.

#### `GET /api/platform/commands`

`operationId: listNodeCommands`. Legacy spelling `GET /api/commands`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_CommandSummary` - One page of commands, newest first.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_CommandSummary`):

```json
{
  "items": [],
  "page": {
    "count": 0,
    "limit": 100,
    "nextCursor": null,
    "total": 0
  },
  "schemaVersion": 1
}
```

#### `POST /api/platform/commands/cancel/{command_id}`

`operationId: cancelNodeCommand`. Legacy spelling `POST /api/commands/cancel/{command_id}`, deprecated.

- **Path** `command_id` (string, required) - The command's UUID.
- **200** `CommandCancelled` - The command was cancelled.
- **Refusals** `400`, `401`, `403`, `404`, `409` - see [Errors](#errors).

#### `POST /api/platform/commands/execute/{node_id}`

`operationId: executeNodeCommand`. Legacy spelling `POST /api/commands/execute/{node_id}`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **Body** (`application/json`, required) `ExecuteCommandRequest`
  - `command_type` (`CommandType`, required) - One of the built-in command names, or `{"custom": "<name>"}`.
  - `data` (null or `Value`, optional)
- **202** `CommandSubmitted` - The command was queued; poll its status.
- **Refusals** `400`, `401`, `403`, `404`, `500`, `503` - see [Errors](#errors).

#### `GET /api/platform/commands/node/{node_id}`

`operationId: listNodeCommandsOfNode`. Legacy spelling `GET /api/commands/node/{node_id}`, deprecated.

- **Path** `node_id` (string, required) - The node id the node announced.
- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer. Opaque, bound to the collection and the filters it was issued for, and refused with 400 `cursor-invalid` anywhere else.
- **200** `ControlListPage_CommandSummary` - One page of the node's commands, newest first.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

#### `GET /api/platform/commands/status/{command_id}`

`operationId: getNodeCommandStatus`. Legacy spelling `GET /api/commands/status/{command_id}`, deprecated.

- **Path** `command_id` (string, required) - The command's UUID.
- **200** `CommandStatusResponse` - The command's status.
- **Refusals** `400`, `401`, `403`, `404` - see [Errors](#errors).

### Events

The control plane's own log of state changes, newest first, filterable by
node and severity. This is what the console shows and what replaces the
WebSocket feed this API never had.

#### `GET /api/platform/events`

`operationId: listEvents`. Legacy spelling `GET /api/events`, deprecated.

- **Query** `limit` (integer, optional) - Page size, `1..=1000`; absent means 100.
- **Query** `cursor` (string, optional) - The `page.nextCursor` of the previous answer, issued for the same filters.
- **Query** `node_id` (string, optional) - The node the event is about, by the id the node announced. An event about no node in particular has none and matches no node filter.
- **Query** `severity` (string, optional) - `Info`, `Warning` or `Error`.
- **200** `ControlListPage_EventItem` - One page of events, newest first.
- **Refusals** `400`, `401`, `403` - see [Errors](#errors).

Example answer (`200`, `ControlListPage_EventItem`):

```json
{
  "items": [],
  "page": {
    "count": 0,
    "limit": 100,
    "nextCursor": null,
    "total": 0
  },
  "schemaVersion": 1
}
```

#### `GET /api/platform/events/stats`

`operationId: getEventStats`. Legacy spelling `GET /api/events/stats`, deprecated.

- **200** `EventStatsResponse` - Counts by severity and by type.
- **Refusals** `401`, `403`, `500` - see [Errors](#errors).

Example answer (`200`, `EventStatsResponse`):

```json
{
  "stats": {
    "by_severity": {},
    "by_type": {},
    "total": 0
  },
  "success": true
}
```

### Metrics summary

The same numbers the Prometheus exposition carries, as JSON, for the console.
The exposition itself stays at `GET /api/metrics` forever; see
[Permanent exemptions](#permanent-exemptions).

#### `GET /api/platform/metrics/summary`

`operationId: getMetricsSummary`. Legacy spelling `GET /api/metrics/summary`, deprecated.

- **200** `MetricsSummaryResponse` - The current counters.
- **Refusals** `401`, `403` - see [Errors](#errors).

Example answer (`200`, `MetricsSummaryResponse`):

```json
{
  "metrics": {
    "nodes": {
      "active": 0,
      "by_type": {},
      "connected": 0,
      "disconnected": 0,
      "registered": 0,
      "total": 0
    },
    "pagination": {
      "cursors": null
    },
    "system": {
      "total_commands": 0,
      "total_events": 0,
      "total_heartbeats": 0,
      "uptime_seconds": 0
    }
  },
  "success": true
}
```

### The runtime fleet

The machines that receive deployments, and what they are running right now -
read live from the node rather than from the catalog.

#### `GET /api/platform/runtime`

A node is on this list because the control plane's configuration file names it or because it registered itself; whether it holds a control channel open has nothing to do with it, and the nodes that do that are a different list at `GET /api/platform/sessions` ([`crate::api::nodes`]).

`lost` is the other half of the same question, and the reason it is served here rather than only appearing inside a deployment's answer: these are nodes that were on the list, stopped refreshing their registration and are presumed to be still running what they last received. Every deploy and every application deletion names them as unreached, so an operator has to be able to see which ones they are. There is no operator-initiated way to clear one any more: `Deregister` on the machine channel (see [Node-facing routes](#node-facing-routes)) only admits a caller who proves the exact certificate of the node being withdrawn, which a machine that is really gone cannot present. A lost registration clears itself once `node_registration_ttl_secs` passes without a refresh, or when the machine comes back and registers again.

`contested` is the third: machines that registered a node id another machine was already answering for and were refused it. They are running this platform's code, nothing can be sent to them, and they are on no other list - so this is where an operator finds out that two machines were given one name. See `crate::runtime_client::ContestedNodeId`.

`operationId: listRuntimeFleet`. Legacy spelling `GET /api/runtime`, deprecated.

- **200** `FleetResponse` - The fleet, the lost nodes and the contested ids.
- **Refusals** `401`, `403` - see [Errors](#errors).

Example answer (`200`, `FleetResponse`):

```json
{
  "contested": [],
  "lost": [],
  "nodes": []
}
```

#### `DELETE /api/platform/runtime/{node}/apps/{app_id}`

This is a node-local operation and changes nothing in the catalog, which is why it is **refused** while the catalog still deploys the application: a node reconciles against its assignment list (`FetchAssignments` on the machine channel, see [Node-facing routes](#node-facing-routes)) on its own timer, that list is built from the catalog and names the same application, and the node loads it straight back. The endpoint used to send the unload anyway and answer `200 {"success": true}`, so an operator was told the application was off a node while the node was already putting it back - and there was nothing in the answer, or anywhere else, to say so. An operation that reports success and then reverses itself is worse than one that refuses, because the refusal is the only version of it an operator can act on.

What it is still for: an application the catalog no longer names but a node is still running - a node that was unreachable when the application was deleted, or one that has drifted. That case is the one where the unload sticks, because the next assignment list will not contain it either.

`operationId: unloadRuntimeApp`. Legacy spelling `DELETE /api/runtime/{node}/apps/{app_id}`, deprecated.

- **Path** `node` (string, required) - The node's id in the fleet.
- **Path** `app_id` (string, required) - The application's id.
- **Query** `environment` (string, optional) - Which environment of the application to take off this node. Omitted means every environment of it. An application on a node is identified by `(app_id, environment)`: production and a preview of the same app are two separate loads with their own version and secrets, so unloading without naming one would otherwise be ambiguous.
- **200** `AppUnloaded` - The node took the application off.
- **409** `UnloadRefused` - The catalog still deploys the application to this node, or the node's key is retired; nothing was sent.
- **422** `AppNotUnloaded` - The node answered but did not unload.
- **503** `UnloadRefused` - The catalog could not be read, so nothing was sent.
- **Refusals** `401`, `403`, `404`, `502` - see [Errors](#errors).

#### `GET /api/platform/runtime/{node}/status`

`operationId: getRuntimeNodeStatus`. Legacy spelling `GET /api/runtime/{node}/status`, deprecated.

- **Path** `node` (string, required) - The node's id in the fleet.
- **Query** `apps` (boolean, optional) - Include per-app details (default true).
- **200** `NodeStatusResponse` - What the node reported.
- **Refusals** `401`, `403`, `404`, `409`, `502` - see [Errors](#errors).

### The global route table

Every route on the platform, across all applications. This is a diagnostic
view; the data plane reads the routing table instead (see
[Node-facing routes](#node-facing-routes)).

#### `GET /api/platform/routes`

Every route the platform knows, highest priority first. This is the whole routing table an edge needs in one call, and it stays unpaginated until the product decides how an edge should read a table in pieces.

`operationId: listAllRoutes`. Legacy spelling `GET /api/routes`, deprecated.

- **200** `GlobalRoutesResponse` - Every route of every application.
- **Refusals** `401`, `403`, `500` - see [Errors](#errors).

Example answer (`200`, `GlobalRoutesResponse`):

```json
{
  "routes": [
    {
      "app_id": "demo",
      "env_id": "018f2518-7e38-7f80-8000-000000000001",
      "host": "demo.example.com",
      "id": "018f2518-7e38-7f80-8000-000000000001",
      "path_prefix": "/",
      "priority": 0
    }
  ],
  "total": 1
}
```

---

## Permanent exemptions

Four things are outside the versioning scheme on purpose. They are not
deprecated, they carry no `Deprecation`, `Sunset` or `Link` headers, they are
not in the OpenAPI document, and they never answer the structured error
envelope. Do not "migrate" them.

<a id="get-prometheus-format-metrics"></a>

### `GET /api/metrics`

The Prometheus exposition, at that path, forever. It is not mirrored under
`/api/v1` and only its JSON sibling - `GET /api/platform/metrics/summary` -
lives on a versioned surface. A scrape config that points at it today keeps
working after the legacy tree is sunset.

It is authenticated like every other read and demands `read:*/*`: the
exposition names no application but counts all of them. Give the scraper an
account of its own -

```bash
deis account create prometheus --role viewer
deis token create --account prometheus --scope 'read:*/*' scrape
# The secret is printed once, at creation.
```

- and put the token in the scrape config's `authorization` block. A direct
scrape from the machine the control plane runs on needs no credential while
`api_auth_enabled` is off, exactly like any other direct local read; a scrape
relayed by a reverse proxy presents a credential.

```
# HELP deis_control_nodes_total Total number of nodes
# TYPE deis_control_nodes_total gauge
deis_control_nodes_total 5

# HELP deis_control_nodes_active Number of active nodes
# TYPE deis_control_nodes_active gauge
deis_control_nodes_active 4

# HELP deis_control_nodes_by_role Number of nodes by type
# TYPE deis_control_nodes_by_role gauge
deis_control_nodes_by_role{type="deis-runtime:1.0.0"} 3
deis_control_nodes_by_role{type="deis-proxy:2.0.0"} 2

# HELP deis_control_uptime_seconds Control uptime in seconds
# TYPE deis_control_uptime_seconds counter
deis_control_uptime_seconds 3600

# HELP deis_control_events_total Total number of events
# TYPE deis_control_events_total counter
deis_control_events_total 150

# HELP deis_control_commands_total Total number of commands
# TYPE deis_control_commands_total counter
deis_control_commands_total 25

# HELP deis_control_heartbeats_total Total number of heartbeats
# TYPE deis_control_heartbeats_total counter
deis_control_heartbeats_total 500
```

### The node-facing routes

Registration, assignments, the routing table and the bundle download. See the
next section.

---

## Node-facing routes

Four calls exist for runtime nodes and for deis-proxy rather than for people,
and none of them are HTTP routes on this listener any more. They are not
mounted here at all: asking this listener for one of them answers `404`, the
same as any other path it does not have, and not `401` - a credential cannot
bring them back, because none is accepted here for them. They live on a
second listener, `[machine]`, which is not optional - a control plane
started with no `[machine]` table refuses to run - and which demands mutual
TLS: a node or a proxy proves a certificate whose one URI SAN is a SPIFFE name
naming exactly itself (`spiffe://<trust domain>/deislet/node/<id>` or
`.../proxy/<id>`), issued and rotated outside this process. `api_auth_enabled`
has no effect on this listener either way - it is a human-listener setting,
and this is not the human listener.

Three of the four are RPCs of `deis.machine.MachineChannel`, defined in
`deis-wire`'s machine proto, because nothing in the compiler checks that two
crates agree on a message shape the way it checks a function signature - a
caller that got a field wrong compiled perfectly and simply never joined the
fleet. Read them there rather than from this document:

| What | Where it is defined |
|---|---|
| Register a node, and withdraw it | `deis.machine.MachineChannel`'s `Register` and `Deregister` |
| What a node should be running | `deis.machine.MachineChannel`'s `FetchAssignments` |
| The whole routing table | `deis.machine.MachineChannel`'s `FetchRoutingTable` |
| Download one version's bundle (`GET`, HTTP) | built by `deis_control::delivery::bundle_url()` from `[machine] public_base_url` |

Every one of these RPCs starts by holding the `node_id` (or `proxy_id`) in the
request against the name the caller's certificate proved, and refuses when
the two disagree - a certificate naming one node cannot ask for another's
anything.
That is new: under the fleet-wide value this replaced, a `node_id` in a path
was never checked against who was asking, because one credential answered for
every machine that held it.

Two of the four decide how much that certificate is worth, which is why none
of them may be opened by a dashboard convenience setting:

- The **assignment list** is what a node should be running: one entry per
  environment that has a deployment, carrying the manifest's variables and
  that environment's **decrypted** secrets, because a node that restarts has
  to come back with the environment it had. Once a caller has proved which
  node it is, the list itself is still identical for every node - it is not
  filtered to what that node happens to be running - so a working certificate
  reads every tenant's plaintext, same as the fleet key did; what changed is
  that reading it now needs a certificate naming that one machine rather than
  a value copied into any node's file. An environment whose secrets will not
  decrypt is left out of the list entirely rather than sent with a hole in
  it; the node then does not run it and an `ERROR` line says which
  application and why.
- **Registration** decides which machines are delivery targets in the first
  place.

The **routing table** carries nothing secret - hostnames, path prefixes, app
ids and environment names, all of which anyone who can make a request through
the edge already sees - but together they are a map of the whole
installation, so it stays behind the same certificate check as the rest of
this channel. deis-proxy fetches it every `routing.control_poll_interval_secs`
seconds (5 by default) and resolves an application from it by host and path
prefix. It has three things `GET /api/platform/routes` next door does not:
every environment's **name** (never its `env_id`, because the name is what
travels in `x-deis-environment`), **every** environment whether or not it has
a route of its own, and a **version** for the table as a whole, a digest of
everything except `generated_at`, so a poller can tell "unchanged" from
"changed" without diffing. The consumer treats the document as all or
nothing: deis-proxy compiles the whole table and swaps it in with one pointer
write, and refuses a document with one unusable entry entirely rather than
applying it with that entry skipped. Half a routing table sends live requests
to the wrong application, which is worse than an old one - which is also why
a failed poll changes nothing and the last accepted table keeps serving.

**`node_preshared_keys` opens none of this any more.** It is optional now,
and while a fleet is still mid-migration it does two smaller things
elsewhere: it is what deis-proxy's gRPC control channel falls back to when
that proxy has no certificate of its own yet, and it is the bearer this
control plane presents on a node's plaintext management plane when that node
has none either. Neither is read on this machine channel - a caller here is
either proved by certificate or refused outright. See
[docs/trust-boundaries.md](../../docs/trust-boundaries.md) for what a proved
certificate opens in full and how the three identity files are rotated.

---

## There is no WebSocket API

This document once described `ws://localhost:8080/api/ws`, and the startup
banner once printed that URL. The route never existed: the only thing behind
it was an in-process broadcaster with no consumer, because the console polls.
Both the banner line and the broadcaster are gone.

What is served instead: state changes are recorded as events and read back
from `GET /api/platform/events`, and the console polls the platform lists and
`GET /api/platform/metrics/summary` on a timer.

---

## Generating a client

[`contracts/openapi/v1.json`](../../contracts/openapi/v1.json) is OpenAPI
3.1, committed, and generated from the handlers. Point any generator at it.

- It describes only `/api/v1` and `/api/platform`. The legacy tree, the
  Prometheus endpoint and the node-facing routes are deliberately absent.
- Regenerate after changing a handler:
  `UPDATE_OPENAPI=1 cargo test -p deis-control --test openapi_drift`, then
  commit the result. The plain test fails when the committed document and the
  code disagree.
- `python3 scripts/check-openapi-compat.py` compares the working document
  against `origin/main` and refuses a breaking change without a major bump:
  a removed path, operation, response property or enum value, a response
  property that stopped being required, a request property or parameter that
  started being required, a changed type, a new security requirement.
- `python3 scripts/generate-openapi-types.py` renders the document as the
  web console's TypeScript client,
  [`crates/deis-control/web/src/generated/control-api.ts`](web/src/generated/control-api.ts):
  one type per schema, and an `operations` interface keyed by `operationId`
  with the parameters, the request body and the responses by status. The
  console's own `lib/api.ts` only aliases those types; it restates no
  response model. `--check` is what CI runs, and it refuses a document whose
  client was not regenerated.
- Every list operation answers a `ControlListPage_*` schema, and every
  operation declares the shared error responses, so a generated client gets
  the envelopes for free.
