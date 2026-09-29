# Control HTTP API

The HTTP API of `deis-control`: what the `deis` CLI, the web console and any
other client call.

**The endpoint reference is the OpenAPI contract,
[`contracts/openapi/v1.json`](../../contracts/openapi/v1.json)**, generated
from the handlers themselves and held to them by a drift test and a
compatibility gate (see [Generating a client](#generating-a-client)). Every
route, parameter, body, response and example is there and nowhere else; the
examples are captured from real handler answers into
[`contracts/openapi/v1-examples.json`](../../contracts/openapi/v1-examples.json).
This page describes only what the contract cannot say on its own: the rules
every route shares. The design decisions behind them are in
[`docs/adr/016-api-cli-web-console.md`](../../docs/adr/016-api-cli-web-console.md);
the words used are defined in [`docs/glossary.md`](../../docs/glossary.md).

**Base URL.** Whatever `deis-control` binds to; `http://127.0.0.1:8085` in
the demo. Every path below is absolute from there.

---

## Surfaces

Two trees are served, and a route belongs to exactly one of them:

| Surface | Prefix | What is there |
|---|---|---|
| Product | `/api/v1` | Applications and everything inside them, accounts, tokens, the audit log, hostname grants, providers, backend restores, long operations. |
| Platform | `/api/platform` | Operator diagnostics: control channels, node configuration and node commands, events, the metrics summary, the runtime fleet, the global route table. |

The major version is spelled in the path, once, as `v1`. There is no version
header and no content negotiation. Within `v1`, changes are additive - a new
field on an answer, a new optional parameter, a new route - and the
compatibility gate refuses anything else without a major bump.

A route under `/api/platform` answers about the installation as a whole; a
route under `/api/v1` answers about an application. Product routes are
refused under `/api/platform` and platform routes under `/api/v1`.

Path parameters are spelled the same on every route: `{app_id}` for an
application, `{env_name}` for an environment, `{node_id}` for a node,
`{account}` for an account.

Outside both trees: `GET /health`, the only route in front of the
authorization gate, which answers the constant `OK`; and `GET /api/metrics`
(below). The machine channel nodes and proxies use is a separate listener
(below).

---

## Request context: `x-request-id` and `x-correlation-id`

Every answer on either tree carries both headers. `x-request-id` identifies
this one request; it is also the `requestId` inside a structured error body
and in the audit line the call produced. `x-correlation-id` identifies a
chain of requests and defaults to the request id.

A caller may choose either id by sending the header, but only as **one**
header occurrence carrying **one** canonical lowercase UUIDv7. Anything else
is discarded and a fresh id is minted. The authorization gate's 401 and 403
and the route fallback's 404 carry `x-request-id` but no `x-correlation-id`.

---

## Authentication

Authentication is always on; there is no anonymous access, on loopback or
anywhere else. Present a token either way:

```bash
curl -H "Authorization: Bearer <token>" http://127.0.0.1:8085/api/v1/apps
curl -H "X-API-Key: <token>"            http://127.0.0.1:8085/api/v1/apps
```

Tokens are minted through `POST /api/v1/accounts/{account}/tokens`
(`deis token create`); each belongs to an account and carries scopes. The
first owner account is created with the one-shot bootstrap token a fresh
control plane writes to `<data_dir>/bootstrap.token`; the README's console
section describes it. A page from another origin can read answers only when
its origin is named in `api_cors_allowed_origins` (`*` is refused there).

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
| Accounts, tokens, audit, hostname grants, providers, every other operation read and cancel, backend restores, retired apps, `DELETE /api/v1/apps/{app_id}`, `GET /api/v1/deployment-plans/{id}` | `admin:*/*` |
| Platform: sessions, events, metrics, node configuration and commands, `GET /api/platform/routes`, `GET /api/platform/runtime/{node_id}/status` | `read:*/*`, and `admin:*/*` for the writes (configuration, commands) |
| `GET /api/platform/runtime` | any scope at all - it is the fleet's health, which `deis status` prints |
| `DELETE /api/platform/runtime/{node_id}/apps/{app_id}` | `deploy:<app>/<env>` with `?environment=<env>`; without it the call takes *every* environment off the node, so it demands `deploy:<app>/*` |

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


Every error on `/api/v1` and `/api/platform` is one JSON object under `error`:

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
### The code table

| Code | HTTP | Retryable | When |
|---|---|---|---|
| `invalid-argument` | 400 | no | Malformed body, query or path value; validation failed. |
| `limit-out-of-range` | 400 | no | A `limit` outside `1..=1000` on any list route. `target` is `{resource: "query", field: "limit"}`. |
| `cursor-invalid` | 400 | no | A cursor that was not issued for this collection and these filters, or has expired. `target` is `{resource: "query", field: "cursor"}`. |
| `idempotency-key-required` | 400 | no | A mutation that replays under a key was sent without one. `target` is `{resource: "request", field: "Idempotency-Key"}`. |
| `unauthenticated` | 401 | no | Nothing usable was presented. From the authorization gate. |
| `insufficient-scope` | 403 | no | Authenticated, but the scope does not cover this call. From the authorization gate. |
| `forbidden` | 403 | no | The call is refused for a reason other than scope. |
| `not-found` | 404 | no | No such application, environment, version, record. |
| `route-not-found` | 404 | no | No such endpoint. From the route fallback. |
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
credential, installing a driver release, starting a backend restore. Those **require** an `Idempotency-Key` request header, and
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

The routes that require a key say so in the OpenAPI document:
every provider credential, driver-release, installation, target and
target-grant mutation,
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
delete, revoke, rotate) and on cancelling an operation. Which ones is
recorded per operation in the OpenAPI document.

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

**Product operations** - backend restore, provider deployment, operation
cancellation. The answer is a `ProductOperation`
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

---

<a id="get-prometheus-format-metrics"></a>

## `GET /api/metrics`

The Prometheus exposition, outside both trees and outside the OpenAPI
document; its JSON sibling is `GET /api/platform/metrics/summary`. It is
authenticated like every other read and demands `read:*/*`. Give the scraper
an account of its own:

```bash
deis account create prometheus --role viewer
deis token create --account prometheus --scope 'read:*/*' scrape
# The secret is printed once, at creation.
```

and put the token in the scrape config's `authorization` block.

---

## The machine channel

Nodes and deis-proxy do not use this listener. They talk to the control
plane over the `[machine]` listener, which demands mutual TLS with a
certificate naming exactly the caller (`spiffe://<trust domain>/deislet/node/<id>`
or `.../proxy/<id>`). Its calls are the RPCs of `deis.machine.MachineChannel`
in `deis-wire`'s machine proto - registration, assignments, the routing
table, secret values, capability issuance - plus the bundle download URL
built by `deis_control::delivery::bundle_url()`. Read them there; see
[docs/trust-boundaries.md](../../docs/trust-boundaries.md) for what a proved
certificate opens.

---

## Generating a client
## Generating a client

[`contracts/openapi/v1.json`](../../contracts/openapi/v1.json) is OpenAPI
3.1, committed, and generated from the handlers. Point any generator at it.

- It describes only `/api/v1` and `/api/platform`. The Prometheus endpoint
  and the machine channel are deliberately absent.
- Regenerate after changing a handler:
  `UPDATE_OPENAPI=1 cargo nextest run -p deis-control -E 'test(openapi)'`, then
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
