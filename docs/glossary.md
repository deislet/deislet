# Glossary

One word per concept. When this page and another document disagree on a
word, this page wins and the other document is the one to fix. ADRs are the
exception: they are dated decision records and keep the words they were
written in, so an ADR saying "backend" or "vendor" means what this page calls
a *target*.

Chinese version: [glossary.zh.md](glossary.zh.md).

## Where applications run

| Term | Meaning | Where it appears | Not to be called |
|---|---|---|---|
| **target** | A platform a project is built for and can be deployed to. The ids are `deislet` (this platform, self-hosted), `cloudflare` (Cloudflare Workers/Pages), and the frozen `deno` (Deno Deploy) and `tencent` (Tencent EdgeOne). | `deis build --target`, `deis deploy --target`, `deis targets`, `docs/product/target-status.json` | vendor, backend, platform (in this sense) |
| **supported target** | `deislet` and `cloudflare`: the targets Deislet builds for, deploys to and works on. | README, `docs/product/README.md` | first-class backend |
| **frozen target** | `deno` and `tencent`: built only when named, stateless functions only (no bindings), no certification promise, no new work. `lifecycle: frozen` in the target status. | `deis build --target deno`, target status | experimental backend |
| **provider** | The company that runs a target other than `deislet`, and the control-plane machinery that deploys there on the operator's behalf. Today the only provider with a driver is Cloudflare. | `/api/v1/providers/*`, `deis-provider-driver*` | vendor, cloud |
| **provider driver** | The process the control plane runs to perform one lifecycle step (preflight, plan, prepare, upload, verify, activate, observe, delete, cleanup) against a provider. | `deis-provider-driver`, `deis-provider-driver-cloudflare`, `contracts/v1/provider-driver-process.schema.json` | adapter (that is a build adapter, below) |
| **provider target** | One registered destination on a provider — an account and its settings — that environments can be granted. Not to be confused with *target*: `cloudflare` is the target, "the ACME Cloudflare account" is a provider target. | `/api/v1/providers/targets`, `deis deploy --provider-target` | provider account |
| **provider target grant** | Permission for one environment to deploy to one provider target. | `/api/v1/apps/{app_id}/environments/{env_name}/provider-target-grants` | — |
| **build adapter** | The crate that turns the canonical build output into one target's payload (`deis-build-adapter-<target>`). | `crates/deis-build-adapter-*` | vendor adapter |

Two spellings are fixed by the Edge Canon standard rather than by Deislet
and are kept as the standard spells them: the `vendors` table in
`.config.json` (one entry per target, keyed by the target id), and the
`backends[]` list in the conformance registry and the target status generated
from it, whose ids are the registry's long form (`cloudflare-workers-pages`,
`tencent-edgeone-makers`). The provider API and the provider driver
contracts carry that same registry id in a `backendId` field (for example
when storing a Cloudflare credential). `deis targets inspect` accepts either
the target id or the registry id.

## Applications and what is deployed

| Term | Meaning | Where it appears | Not to be called |
|---|---|---|---|
| **app** (application) | The unit a person deploys: one project, one id. In API paths it is always `{app_id}`. | `/api/v1/apps/{app_id}`, `deis --app` | tenant (in prose), site |
| **environment** | A named slot of an app (`production`, `pr-42`) with its own pointer, secrets, routes and data. In API paths it is always `{env_name}`. | `/environments/{env_name}`, `deis -e, --environment` | stage |
| **tenant** | In code only: the data namespace of an app's storage, keyed by its canonical id. Documentation says *app*. | `deis_wire::conventions`, storage capability claims | — |
| **artifact** | The Edge Canon canonical build output of one build (`edge-canon.build-output/v1`), recorded by the control plane. Versions and provider payloads are derived from it. | `/apps/{app_id}/artifacts`, `deis-artifact` | build |
| **bundle** | The `deislet` target's payload: one zip a node can run. | `deis-bundle`, `bundle.zip` | package |
| **provider payload** | A non-`deislet` target's payload, derived from an artifact and handed to a provider driver. | `/artifacts/{artifact_id}/provider-payloads` | — |
| **version** | An immutable, uploaded bundle of an app, identified by its digest. | `deis versions`, `/apps/{app_id}/versions` | release, revision |
| **binding** | A named resource a project declares under `bindings` in `.config.json` (`kv`, `database`, `blob`, `queue`, `service`) and reaches as `context.env.<NAME>`. `context.env` is the only standard spelling; the build refuses `context.bindings` and `context.services` in application source. | `.config.json`, `context.env.<NAME>` | service (for a storage binding) |

## Deploying

| Term | Meaning | Where it appears | Not to be called |
|---|---|---|---|
| **deployment** | Pointing an environment of this platform at a version. One write; its history is what `deis deployments` lists. | `deis deploy`, `/environments/{env_name}/deployments` | release, rollout |
| **deployment plan** | The immutable per-node plan the control plane computes for a deployment, and whose prepare/activate/drain it tracks. | `/deployment-plans/{id}` | — |
| **generation** | The number a node gives each deployment it holds for an environment, so the proxy can pin a request to one. | `x-deis-deployment-generation` | revision |
| **provider deployment** | A durable workflow that drives a provider driver through its steps for one environment. It can be in progress, cancelled, or rolled back — none of which is true of a deployment. | `deis provider-deployments`, `/provider-deployments` | deployment (unqualified) |
| **operation** | The generic record of a long-running, durable job. A provider deployment is one kind of operation. | `/api/v1/operations/{operation_id}` | job, task |
| **revision** | The compare-and-swap counter of one record (sent as `ETag`/`If-Match`), and the numbered versions of a secret. | `ETag`, secret revisions | generation, version |

## The platform's own parts

| Term | Meaning |
|---|---|
| **control plane** | `deis-control`: one instance, one SQLite database, backed up by Litestream. The only authority on metadata. |
| **node** | `deis-runtime`: runs the apps it is assigned. |
| **proxy** | `deis-proxy`: the edge that terminates TLS and routes a request by host and path through the control plane's routing table. |
| **data services** | `deis-store`, `deis-queue`, `deis-telemetry`. |
| **capability** | A short-lived token the control plane signs for one purpose (storage, queue, telemetry) and one scope; `deis-capability` verifies it. |
